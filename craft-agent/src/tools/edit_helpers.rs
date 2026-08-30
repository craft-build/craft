const LF: &str = "\n";
const CRLF: &str = "\r\n";

// A tie stays LF, so the answer never depends on which ending the file happens
// to show first.
fn dominant_ending(content: &str) -> &'static str {
    let crlf = content.matches(CRLF).count();
    let lf = content.matches('\n').count();
    if crlf > lf - crlf { CRLF } else { LF }
}

// The `read` tool strips `\r`, so the model can only ever hand back LF text.
// Applied straight to a CRLF file, an edit rewrites the lines it touches as LF
// and leaves the rest alone, so the file ends up with both endings in it. Every
// edit tool runs its transform on LF content here, and the file goes back out
// with the ending it came with.
pub fn preserve_line_endings<F>(content: &str, transform: F) -> Result<String, String>
where
    F: FnOnce(&str) -> Result<String, String>,
{
    let ending = dominant_ending(content);
    let result = transform(&content.replace(CRLF, LF))?;
    if ending == LF {
        return Ok(result);
    }
    Ok(result.replace(CRLF, LF).replace('\n', ending))
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    fn replace_line_2(lf: &str, replacement: &str) -> Result<String, String> {
        let trailing = lf.ends_with(LF);
        let lines: Vec<&str> = lf.strip_suffix(LF).unwrap_or(lf).split(LF).collect();
        let replaced: Vec<String> = lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if i == 1 {
                    replacement.to_owned()
                } else {
                    (*l).to_owned()
                }
            })
            .collect();
        let joined = replaced.join(LF);
        Ok(if trailing {
            format!("{joined}{LF}")
        } else {
            joined
        })
    }

    fn edit(content: &str, replacement: &str) -> String {
        preserve_line_endings(content, |lf| replace_line_2(lf, replacement)).unwrap()
    }

    #[test_case("aaa\r\nbbb\r\nccc\r\n", "XXX\nYYY" => "aaa\r\nXXX\r\nYYY\r\nccc\r\n" ; "crlf_kept")]
    #[test_case("aaa\nbbb\nccc\n", "XXX" => "aaa\nXXX\nccc\n" ; "lf_kept")]
    #[test_case("aaa\r\nbbb", "XXX" => "aaa\r\nXXX" ; "crlf_without_trailing_newline")]
    #[test_case("a\r\nb\r\nc\r\nd\n", "X" => "a\r\nX\r\nc\r\nd\r\n" ; "majority_crlf")]
    #[test_case("a\r\nb\n", "X" => "a\nX\n" ; "tie_stays_lf")]
    fn keeps_the_files_own_ending(content: &str, replacement: &str) -> String {
        edit(content, replacement)
    }
}
