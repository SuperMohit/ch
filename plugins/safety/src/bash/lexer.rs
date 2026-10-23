use super::{Op, Segment, is_harmless_target};

pub(super) fn split(src: &str) -> (Vec<Segment>, Vec<String>) {
    let mut lx = Lexer::new(src);
    lx.run();
    (lx.segments, lx.substitution_bodies)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Redirect {
    Output,
    /// `>&`: the target is an fd, or a file for `>&file`.
    Dup,
    Input,
    Heredoc,
    HereString,
}

struct Lexer<'a> {
    src: &'a str,
    chars: Vec<(usize, char)>,
    pos: usize,
    segments: Vec<Segment>,
    substitution_bodies: Vec<String>,
    seg_start: usize,
    words: Vec<String>,
    word: Option<String>,
    op_before: Option<Op>,
    has_substitution: bool,
    writes_file: bool,
    redirect: Option<Redirect>,
    heredocs: Vec<String>,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            chars: src.char_indices().collect(),
            pos: 0,
            segments: Vec::new(),
            substitution_bodies: Vec::new(),
            seg_start: 0,
            words: Vec::new(),
            word: None,
            op_before: None,
            has_substitution: false,
            writes_file: false,
            redirect: None,
            heredocs: Vec::new(),
        }
    }

    fn peek(&self, off: usize) -> Option<char> {
        self.chars.get(self.pos + off).map(|&(_, c)| c)
    }

    fn byte_at(&self, pos: usize) -> usize {
        self.chars.get(pos).map_or(self.src.len(), |&(b, _)| b)
    }

    fn push_char(&mut self, c: char) {
        self.word.get_or_insert_with(String::new).push(c);
    }

    fn end_word(&mut self) {
        let Some(w) = self.word.take() else {
            return;
        };
        match self.redirect.take() {
            None => self.words.push(w),
            Some(Redirect::Output) => {
                if !is_harmless_target(&w) {
                    self.writes_file = true;
                }
            }
            Some(Redirect::Dup) => {
                if !(w == "-" || w.chars().all(|c| c.is_ascii_digit()) || is_harmless_target(&w)) {
                    self.writes_file = true;
                }
            }
            Some(Redirect::Input) | Some(Redirect::HereString) => {}
            Some(Redirect::Heredoc) => self.heredocs.push(w),
        }
    }

    fn end_segment(&mut self, end_pos: usize, next_op: Option<Op>) {
        self.end_word();
        let raw = self.src[self.seg_start..self.byte_at(end_pos)]
            .trim()
            .to_string();
        if !self.words.is_empty() || !raw.is_empty() {
            self.segments.push(Segment {
                raw,
                words: std::mem::take(&mut self.words),
                op_before: self.op_before,
                has_substitution: self.has_substitution,
                writes_file: self.writes_file,
            });
        }
        self.op_before = next_op;
        self.has_substitution = false;
        self.writes_file = false;
        self.redirect = None;
    }

    fn operator(&mut self, len: usize, op: Op) {
        let at = self.pos;
        self.end_segment(at, Some(op));
        self.pos += len;
        self.seg_start = self.byte_at(self.pos);
    }

    fn run(&mut self) {
        while let Some(c) = self.peek(0) {
            match c {
                '\'' => {
                    self.word.get_or_insert_with(String::new);
                    self.pos += 1;
                    while let Some(ch) = self.peek(0) {
                        self.pos += 1;
                        if ch == '\'' {
                            break;
                        }
                        self.push_char(ch);
                    }
                }
                '"' => {
                    self.word.get_or_insert_with(String::new);
                    self.pos += 1;
                    while let Some(ch) = self.peek(0) {
                        match ch {
                            '"' => {
                                self.pos += 1;
                                break;
                            }
                            '\\' => {
                                if let Some(n) = self.peek(1) {
                                    if matches!(n, '"' | '\\' | '$' | '`') {
                                        self.push_char(n);
                                    } else if n != '\n' {
                                        self.push_char('\\');
                                        self.push_char(n);
                                    }
                                    self.pos += 2;
                                } else {
                                    self.pos += 1;
                                }
                            }
                            '$' if self.peek(1) == Some('(') => self.substitution(),
                            '`' => self.backtick(),
                            _ => {
                                self.push_char(ch);
                                self.pos += 1;
                            }
                        }
                    }
                }
                '\\' => {
                    match self.peek(1) {
                        Some('\n') => {}
                        Some(n) => self.push_char(n),
                        None => {}
                    }
                    self.pos += 2;
                }
                '$' if self.peek(1) == Some('(') => self.substitution(),
                '<' | '>' if self.peek(1) == Some('(') => self.substitution(),
                '`' => self.backtick(),
                '#' if self.word.is_none() => {
                    while let Some(ch) = self.peek(0) {
                        if ch == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                ' ' | '\t' | '\r' => {
                    self.end_word();
                    self.pos += 1;
                }
                '\n' => {
                    self.operator(1, Op::Seq);
                    self.skip_heredoc_bodies();
                }
                ';' => {
                    let len = if self.peek(1) == Some(';') { 2 } else { 1 };
                    self.operator(len, Op::Seq);
                }
                '(' | ')' => self.operator(1, Op::Seq),
                '|' => match self.peek(1) {
                    Some('|') => self.operator(2, Op::Or),
                    Some('&') => self.operator(2, Op::Pipe),
                    _ => self.operator(1, Op::Pipe),
                },
                '&' => match self.peek(1) {
                    Some('&') => self.operator(2, Op::And),
                    Some('>') => {
                        self.end_word();
                        self.pos += 2;
                        if self.peek(0) == Some('>') {
                            self.pos += 1;
                        }
                        self.redirect = Some(Redirect::Output);
                    }
                    _ => self.operator(1, Op::Background),
                },
                '>' | '<' => self.redirection(c),
                _ => {
                    self.push_char(c);
                    self.pos += 1;
                }
            }
        }
        let end = self.chars.len();
        self.end_segment(end, None);
    }

    fn redirection(&mut self, c: char) {
        // A pure-digit word right before the operator is an fd number (`2>`).
        if self
            .word
            .as_ref()
            .is_some_and(|w| !w.is_empty() && w.chars().all(|d| d.is_ascii_digit()))
        {
            self.word = None;
        }
        self.end_word();
        self.pos += 1;
        let kind = if c == '>' {
            match self.peek(0) {
                Some('>') | Some('|') => {
                    self.pos += 1;
                    Redirect::Output
                }
                Some('&') => {
                    self.pos += 1;
                    Redirect::Dup
                }
                _ => Redirect::Output,
            }
        } else {
            match self.peek(0) {
                Some('<') => {
                    self.pos += 1;
                    if self.peek(0) == Some('<') {
                        self.pos += 1;
                        Redirect::HereString
                    } else {
                        if self.peek(0) == Some('-') {
                            self.pos += 1;
                        }
                        Redirect::Heredoc
                    }
                }
                Some('&') => {
                    self.pos += 1;
                    Redirect::Input
                }
                Some('>') => {
                    // `<>` opens read-write.
                    self.pos += 1;
                    Redirect::Output
                }
                _ => Redirect::Input,
            }
        };
        self.redirect = Some(kind);
    }

    fn substitution(&mut self) {
        self.has_substitution = true;
        let start = self.pos;
        self.pos += 2;
        let body_start = self.pos;
        let mut depth = 1usize;
        let mut quote: Option<char> = None;
        while let Some(ch) = self.peek(0) {
            match (quote, ch) {
                (Some(q), c) if c == q => quote = None,
                (Some('"'), '\\') => self.pos += 1,
                (Some(_), _) => {}
                (None, '\'' | '"') => quote = Some(ch),
                (None, '\\') => self.pos += 1,
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        self.end_substitution(start, body_start);
    }

    fn backtick(&mut self) {
        self.has_substitution = true;
        let start = self.pos;
        self.pos += 1;
        let body_start = self.pos;
        while let Some(ch) = self.peek(0) {
            if ch == '\\' {
                self.pos += 2;
                continue;
            }
            if ch == '`' {
                break;
            }
            self.pos += 1;
        }
        self.end_substitution(start, body_start);
    }

    fn end_substitution(&mut self, start: usize, body_start: usize) {
        let body_end = self.pos.min(self.chars.len());
        let body = self.src[self.byte_at(body_start)..self.byte_at(body_end)].to_string();
        self.pos = (self.pos + 1).min(self.chars.len());
        let text = self.src[self.byte_at(start)..self.byte_at(self.pos)].to_string();
        for ch in text.chars() {
            self.push_char(ch);
        }
        self.substitution_bodies.push(body);
    }

    fn skip_heredoc_bodies(&mut self) {
        let delims = std::mem::take(&mut self.heredocs);
        for delim in delims {
            loop {
                let line_start = self.pos;
                let mut line_end = self.pos;
                while let Some(ch) = self.chars.get(line_end).map(|&(_, c)| c) {
                    if ch == '\n' {
                        break;
                    }
                    line_end += 1;
                }
                let line = &self.src[self.byte_at(line_start)..self.byte_at(line_end)];
                self.pos = (line_end + 1).min(self.chars.len());
                if line.trim() == delim || self.pos >= self.chars.len() {
                    break;
                }
            }
        }
        self.seg_start = self.byte_at(self.pos);
    }
}
