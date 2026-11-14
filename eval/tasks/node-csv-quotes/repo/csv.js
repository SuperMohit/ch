'use strict';

// Splits one line of CSV into its fields.
function parseLine(line) {
  return line.split(',');
}

// Parses CSV text into rows of fields. Blank lines are skipped.
function parse(text) {
  return text
    .split(/\r?\n/)
    .filter((line) => line.trim() !== '')
    .map(parseLine);
}

module.exports = { parseLine, parse };
