//! The Rust edition a file is parsed with.

use std::path::Path;

use toml::{Table, Value};

/// For a file that belongs to no package.
const OUTSIDE_A_PACKAGE: &str = "2021";

/// Cargo's edition for a package that doesn't name one.
const UNNAMED: &str = "2015";

/// The edition of the package `rel` belongs to: the nearest `Cargo.toml` with a
/// `[package]`, from `rel`'s directory up to `root`. `edition.workspace = true` is
/// followed up to the `[workspace.package]` that sets it.
///
/// The value goes into a shell command, so anything but four digits is ignored.
pub fn rust_edition(root: &Path, rel: &str) -> String {
    let mut dir = root.join(rel);
    let mut inherited = false;
    while dir.pop() && dir.starts_with(root) {
        let Some(doc) = std::fs::read_to_string(dir.join("Cargo.toml"))
            .ok()
            .and_then(|text| toml::from_str::<Table>(&text).ok())
        else {
            continue;
        };
        if !inherited {
            let Some(package) = doc.get("package").and_then(Value::as_table) else {
                continue;
            };
            match package.get("edition") {
                None => return UNNAMED.into(),
                Some(Value::String(e)) => return valid(e),
                Some(Value::Table(t)) if t.get("workspace") == Some(&Value::Boolean(true)) => {
                    inherited = true;
                }
                Some(_) => return OUTSIDE_A_PACKAGE.into(),
            }
        }
        // The package's own manifest may also be the workspace root.
        if let Some(workspace) = doc.get("workspace").and_then(Value::as_table) {
            return match workspace
                .get("package")
                .and_then(|p| p.get("edition"))
                .and_then(Value::as_str)
            {
                Some(e) => valid(e),
                None => OUTSIDE_A_PACKAGE.into(),
            };
        }
    }
    OUTSIDE_A_PACKAGE.into()
}

fn valid(edition: &str) -> String {
    if edition.len() == 4 && edition.bytes().all(|b| b.is_ascii_digit()) {
        edition.to_string()
    } else {
        OUTSIDE_A_PACKAGE.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn the_nearest_package_decides() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        write(
            ws,
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n[workspace.package]\nedition = \"2024\"\n",
        );
        write(
            ws,
            "crates/a/Cargo.toml",
            "[package]\nname = \"a\"\nedition.workspace = true\n",
        );
        write(
            ws,
            "crates/b/Cargo.toml",
            "[package]\nname = \"b\"\nedition = \"2018\"\n",
        );
        write(ws, "crates/c/Cargo.toml", "[package]\nname = \"c\"\n");
        write(
            ws,
            "crates/d/Cargo.toml",
            "[package]\nedition = \"2021; touch pwned\"\n",
        );
        write(ws, "crates/e/Cargo.toml", "this is not toml [");

        assert_eq!(rust_edition(ws, "crates/a/src/deep/lib.rs"), "2024");
        assert_eq!(rust_edition(ws, "crates/b/src/lib.rs"), "2018");
        assert_eq!(rust_edition(ws, "crates/c/src/lib.rs"), "2015");
        assert_eq!(rust_edition(ws, "crates/d/src/lib.rs"), "2021");
        // An unreadable manifest is skipped; the root has no [package].
        assert_eq!(rust_edition(ws, "crates/e/src/lib.rs"), "2021");
        assert_eq!(rust_edition(ws, "scripts/x.rs"), "2021");
    }

    #[test]
    fn a_root_package_that_is_also_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        write(
            ws,
            "Cargo.toml",
            "[package]\nname = \"r\"\nedition.workspace = true\n\
             [workspace]\n[workspace.package]\nedition = \"2024\"\n",
        );
        assert_eq!(rust_edition(ws, "src/main.rs"), "2024");
    }
}
