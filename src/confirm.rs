//! Interactive yes/no confirmation for destructive commands.

use crate::error::Result;
use std::io::{BufRead, Write};

/// Ask a yes/no question on stderr and read the answer from stdin. Any
/// answer other than `y`/`yes` (including EOF) is a no.
pub fn ask(question: &str) -> Result<bool> {
    let mut stdin = std::io::stdin().lock();
    let mut stderr = std::io::stderr().lock();
    ask_with(&mut stdin, &mut stderr, question)
}

pub fn ask_with<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    question: &str,
) -> Result<bool> {
    write!(writer, "{question} [y/N] ")?;
    writer.flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(input: &str) -> bool {
        let mut reader = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut writer: Vec<u8> = Vec::new();
        ask_with(&mut reader, &mut writer, "Proceed?").unwrap()
    }

    #[test]
    fn accepts_yes_variants() {
        assert!(answered("y\n"));
        assert!(answered("Y\n"));
        assert!(answered("yes\n"));
        assert!(answered("YES\n"));
    }

    #[test]
    fn rejects_everything_else() {
        assert!(!answered("n\n"));
        assert!(!answered("\n"));
        assert!(!answered("yolo\n"));
        assert!(!answered(""), "EOF is a no");
    }

    #[test]
    fn prints_the_question() {
        let mut reader = std::io::Cursor::new(b"y\n".to_vec());
        let mut writer: Vec<u8> = Vec::new();
        ask_with(&mut reader, &mut writer, "Proceed?").unwrap();
        assert_eq!(String::from_utf8(writer).unwrap(), "Proceed? [y/N] ");
    }
}
