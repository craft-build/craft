use std::path::PathBuf;

const SED: &str = "sed";
const PERL: &str = "perl";

const END_OF_OPTIONS: &str = "--";
const IN_PLACE_LONG: &str = "--in-place";

const GLOB_CHARS: &[char] = &['*', '?', '[', '{'];

fn has_glob_chars(s: &str) -> bool {
    s.chars().any(|c| GLOB_CHARS.contains(&c))
}

fn has_command_substitution(s: &str) -> bool {
    s.contains('$') || s.contains('`')
}

fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

struct Tokenizer {
    chars: Vec<char>,
    pos: usize,
}

impl Tokenizer {
    fn new(s: &str) -> Self {
        Self {
            chars: s.chars().collect(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn next_token(&mut self) -> Option<(String, Quote)> {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.bump();
                continue;
            }
            if c == '#' {
                while let Some(cc) = self.peek() {
                    if cc == '\n' {
                        break;
                    }
                    self.bump();
                }
                continue;
            }
            return Some(self.read_token());
        }
        None
    }

    fn read_token(&mut self) -> (String, Quote) {
        let mut buf = String::new();
        let mut quote = Quote::None;

        while let Some(c) = self.peek() {
            match c {
                '\\' => {
                    self.bump();
                    if let Some(escaped) = self.bump() {
                        buf.push(escaped);
                    }
                }
                '\'' if quote != Quote::Double => {
                    quote = toggle(quote, Quote::Single);
                    self.bump();
                }
                '"' if quote != Quote::Single => {
                    quote = toggle(quote, Quote::Double);
                    self.bump();
                }
                ';' | '|' | '&' if quote == Quote::None => {
                    if buf.is_empty() {
                        buf.push(c);
                        self.bump();
                        if c == '&' && self.peek() == Some('&') {
                            buf.push('&');
                            self.bump();
                        }
                    }
                    break;
                }
                w if w.is_whitespace() && quote == Quote::None => {
                    self.bump();
                    break;
                }
                _ => {
                    buf.push(c);
                    self.bump();
                }
            }
        }

        let was_quoted = matches!(quote, Quote::Single | Quote::Double);
        (buf, if was_quoted { quote } else { Quote::None })
    }
}

fn toggle(current: Quote, target: Quote) -> Quote {
    if current == target {
        Quote::None
    } else {
        target
    }
}

fn is_separator(tok: &str, quote: Quote) -> bool {
    quote == Quote::None && matches!(tok, "&&" | ";" | "|")
}

fn split_leaves(s: &str) -> Vec<Vec<(String, Quote)>> {
    let mut leaves: Vec<Vec<(String, Quote)>> = Vec::new();
    let mut current: Vec<(String, Quote)> = Vec::new();
    let mut tokenizer = Tokenizer::new(s);

    while let Some((tok, q)) = tokenizer.next_token() {
        if is_separator(&tok, q) {
            if !current.is_empty() {
                leaves.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push((tok, q));
    }

    if !current.is_empty() {
        leaves.push(current);
    }
    leaves
}

fn detect_leaf(tokens: &[(String, Quote)]) -> Vec<PathBuf> {
    let Some((program, _)) = tokens.first() else {
        return Vec::new();
    };

    if tokens.iter().any(|(t, _)| has_command_substitution(t)) {
        return Vec::new();
    }

    match program.as_str() {
        SED => extract_sed(&tokens[1..]),
        PERL => extract_perl(&tokens[1..]),
        _ => Vec::new(),
    }
}

fn extract_sed(args: &[(String, Quote)]) -> Vec<PathBuf> {
    let mut in_place = false;
    let mut script_consumed = false;
    let mut end_of_options = false;
    let mut files = Vec::new();
    let mut i = 0;

    while let Some((raw, q)) = args.get(i) {
        let arg = unquote(raw);

        if end_of_options {
            if *q == Quote::None && arg == END_OF_OPTIONS {
                i += 1;
                continue;
            }
            if !script_consumed {
                script_consumed = true;
            } else {
                push_file_or_fail(arg, *q, &mut files);
            }
            i += 1;
            continue;
        }

        if *q == Quote::None && arg == END_OF_OPTIONS {
            end_of_options = true;
            i += 1;
            continue;
        }

        if *q == Quote::None && arg.starts_with("--") {
            if arg == IN_PLACE_LONG || arg.starts_with("--in-place=") {
                in_place = true;
            }
            i += 1;
            continue;
        }

        if *q == Quote::None && is_short_flag_cluster(arg) {
            if arg.chars().any(|c| c == 'i') {
                in_place = true;
            }
            // -e/-f supply the script as the following argument, so no
            // positional script slot is expected afterwards.
            if arg == "-e" || arg == "-f" {
                script_consumed = true;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if !script_consumed {
            script_consumed = true;
            i += 1;
            continue;
        }

        push_file_or_fail(arg, *q, &mut files);
        i += 1;
    }

    if in_place { files } else { Vec::new() }
}

fn is_short_flag_cluster(arg: &str) -> bool {
    arg.len() >= 2 && arg.starts_with('-') && arg != "-" && arg != "--"
}

fn extract_perl(args: &[(String, Quote)]) -> Vec<PathBuf> {
    let mut in_place = false;
    let mut script_consumed = false;
    let mut files = Vec::new();
    let mut i = 0;

    while let Some((raw, q)) = args.get(i) {
        let arg = unquote(raw);

        if *q == Quote::None && is_short_flag_cluster(arg) {
            let body = &arg[1..];
            if body.starts_with('i') {
                in_place = true;
            }
            // -e supplies the expression as the following argument, so no
            // positional script slot is expected afterwards.
            if arg == "-e" {
                script_consumed = true;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        if !script_consumed {
            script_consumed = true;
            i += 1;
            continue;
        }

        push_file_or_fail(arg, *q, &mut files);
        i += 1;
    }

    if in_place { files } else { Vec::new() }
}

fn push_file_or_fail(arg: &str, quote: Quote, files: &mut Vec<PathBuf>) {
    if quote == Quote::None && is_short_flag_cluster(arg) {
        return;
    }
    if has_glob_chars(arg) {
        return;
    }
    let path = PathBuf::from(arg);
    if !files.contains(&path) {
        files.push(path);
    }
}

/// Best-effort detection of files rewritten in place by `sed -i` / `perl -i`.
///
/// Returns no paths on any ambiguity (command substitution, globs, unknown
/// flags, unrecognized program): the snapshot layer fails open for safety
/// rather than risk backing up the wrong path or trusting untrusted expansion.
pub(crate) fn detect_inplace_edit_paths(command: &str) -> Vec<PathBuf> {
    split_leaves(command)
        .into_iter()
        .flat_map(|tokens| detect_leaf(&tokens))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const FILE_TXT: &str = "file.txt";
    const ONE_TXT: &str = "one.txt";
    const TWO_TXT: &str = "two.txt";

    fn paths(cmd: &str) -> Vec<String> {
        detect_inplace_edit_paths(cmd)
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    }

    #[test_case("sed -i 's/a/b/' file.txt", vec![FILE_TXT.to_string()] ; "sed_in_place_short")]
    #[test_case("sed -i.bak 's/a/b/' file.txt", vec![FILE_TXT.to_string()] ; "sed_in_place_with_suffix")]
    #[test_case("sed --in-place 's/a/b/' file.txt", vec![FILE_TXT.to_string()] ; "sed_in_place_long")]
    #[test_case("sed --in-place=.bak 's/a/b/' file.txt", vec![FILE_TXT.to_string()] ; "sed_in_place_long_with_suffix")]
    #[test_case("sed -i -e 's/a/b/' file.txt", vec![FILE_TXT.to_string()] ; "sed_script_via_e")]
    #[test_case(
        "sed -i -e 's/a/b/' -e 's/c/d/' file.txt",
        vec![FILE_TXT.to_string()] ; "sed_multiple_expressions_via_e"
    )]
    #[test_case(
        "sed -i -e 's/a/b/' -e 's/c/d/' one.txt two.txt",
        vec![ONE_TXT.to_string(), TWO_TXT.to_string()] ; "sed_multiple_expressions_multiple_files"
    )]
    #[test_case("sed -i 's/a/b/' one.txt two.txt", vec![ONE_TXT.to_string(), TWO_TXT.to_string()] ; "sed_multiple_files")]
    #[test_case(
        "sed -i 's/a/b/' file.txt file.txt",
        vec![FILE_TXT.to_string()] ; "sed_repeated_file_arg_deduped"
    )]
    #[test_case("sed -i -- 's/a/b/' -- file.txt", vec![FILE_TXT.to_string()] ; "sed_end_of_options")]
    fn sed_detects(cmd: &str, expected: Vec<String>) {
        assert_eq!(paths(cmd), expected);
    }

    #[test_case("perl -i -pe 's/a/b/' file.txt", vec![FILE_TXT.to_string()] ; "perl_in_place")]
    #[test_case("perl -i.bak -pe 's/a/b/' file.txt", vec![FILE_TXT.to_string()] ; "perl_in_place_with_suffix")]
    #[test_case("perl -ipe 's/a/b/' file.txt", vec![FILE_TXT.to_string()] ; "perl_combined_flags")]
    #[test_case(
        "perl -i -e 's/a/b/' -e 's/c/d/' file.txt",
        vec![FILE_TXT.to_string()] ; "perl_multiple_expressions_via_e"
    )]
    fn perl_detects(cmd: &str, expected: Vec<String>) {
        assert_eq!(paths(cmd), expected);
    }

    #[test]
    fn pipeline_detects_sed_leaf_only() {
        assert_eq!(paths("echo hi | sed -i 's/a/b/' file.txt"), vec![FILE_TXT]);
    }

    #[test]
    fn chained_and_detects_both_leaves() {
        assert_eq!(
            paths("sed -i 's/a/b/' one.txt && sed -i 's/c/d/' two.txt"),
            vec![ONE_TXT, TWO_TXT]
        );
    }

    #[test]
    fn sed_without_in_place_returns_empty() {
        assert!(paths("sed 's/a/b/' file.txt").is_empty());
    }

    #[test]
    fn glob_target_returns_empty() {
        assert!(paths("sed -i 's/a/b/' *.txt").is_empty());
    }

    #[test]
    fn command_substitution_returns_empty() {
        assert!(paths("sed -i 's/a/b/' \"$(cmd)\"").is_empty());
    }

    #[test]
    fn backtick_substitution_returns_empty() {
        assert!(paths("sed -i 's/a/b/' `cmd`").is_empty());
    }

    #[test]
    fn echo_returns_empty() {
        assert!(paths("echo x").is_empty());
    }

    #[test]
    fn quoted_glob_target_returns_empty() {
        assert!(paths("sed -i 's/a/b/' '*.txt'").is_empty());
    }

    #[test]
    fn perl_without_in_place_returns_empty() {
        assert!(paths("perl -pe 's/a/b/' file.txt").is_empty());
    }

    #[test]
    fn semicolon_separator_splits_leaves() {
        assert_eq!(
            paths("sed -i 's/a/b/' one.txt; sed -i 's/c/d/' two.txt"),
            vec![ONE_TXT.to_string(), TWO_TXT.to_string()]
        );
    }

    #[test]
    fn double_quoted_path_is_unquoted() {
        assert_eq!(paths("sed -i 's/a/b/' \"file.txt\""), vec![FILE_TXT]);
    }
}
