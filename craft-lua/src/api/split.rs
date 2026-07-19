use mlua::{Function, Lua, Result as LuaResult, Table, Value};

const OPTS_TYPE_MSG: &str = "split: opts must be a table or boolean";
const INFINITE_LOOP_MSG: &str = "split: separator matched an empty string (infinite loop)";

/// Split {s} at each occurrence of {sep} and return the pieces as a
/// list. Mirrors Neovim's `vim.split`, so code using it can be copied
/// between Neovim and craft. {sep} is a Lua pattern unless `plain` is
/// set; an empty {sep} splits into single characters.
fn split(lua: &Lua, s: mlua::String, sep: mlua::String, opts: Option<Value>) -> LuaResult<Table> {
    let (plain, trimempty) = parse_opts(opts)?;
    let bytes = s.as_bytes();
    let mut parts: Vec<mlua::String> = Vec::new();

    if sep.as_bytes().is_empty() {
        for i in 0..bytes.len() {
            parts.push(lua.create_string(&bytes[i..=i])?);
        }
        return lua.create_sequence_from(parts);
    }

    let find: Function = lua.globals().get::<Table>("string")?.get("find")?;
    let mut start = 1i64;
    while let (Some(i), Some(j)) =
        find.call::<(Option<i64>, Option<i64>)>((&s, &sep, start, plain))?
    {
        if j < start {
            return Err(mlua::Error::runtime(INFINITE_LOOP_MSG));
        }
        parts.push(lua.create_string(&bytes[start as usize - 1..i as usize - 1])?);
        start = j + 1;
    }
    parts.push(lua.create_string(&bytes[start as usize - 1..])?);

    if trimempty {
        while parts.last().is_some_and(|p| p.as_bytes().is_empty()) {
            parts.pop();
        }
        let leading = parts.iter().take_while(|p| p.as_bytes().is_empty()).count();
        parts.drain(..leading);
    }
    lua.create_sequence_from(parts)
}

fn parse_opts(opts: Option<Value>) -> LuaResult<(bool, bool)> {
    match opts {
        None | Some(Value::Nil) => Ok((false, false)),
        Some(Value::Boolean(plain)) => Ok((plain, false)),
        Some(Value::Table(t)) => Ok((
            t.get::<bool>("plain").unwrap_or(false),
            t.get::<bool>("trimempty").unwrap_or(false),
        )),
        Some(_) => Err(mlua::Error::runtime(OPTS_TYPE_MSG)),
    }
}

pub(crate) fn register(craft: &Table, lua: &Lua) -> LuaResult<()> {
    craft.set(
        "split",
        lua.create_function(
            |lua, (s, sep, opts): (mlua::String, mlua::String, Option<Value>)| {
                split(lua, s, sep, opts)
            },
        )?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn eval(chunk: &str) -> mlua::Result<Vec<String>> {
        let lua = Lua::new();
        let craft = lua.create_table().unwrap();
        register(&craft, &lua).unwrap();
        lua.globals().set("craft", craft).unwrap();
        lua.load(chunk).eval()
    }

    #[test_case(r#"craft.split("a,b,c", ",")"#, &["a", "b", "c"] ; "basic")]
    #[test_case(r#"craft.split(":aa::b:", ":")"#, &["", "aa", "", "b", ""] ; "keeps_empty_pieces")]
    #[test_case(r#"craft.split(":aa::b:", ":", { trimempty = true })"#, &["aa", "", "b"] ; "trimempty_only_trims_ends")]
    #[test_case(r#"craft.split("x*y*z", "*", { plain = true })"#, &["x", "y", "z"] ; "plain_disables_patterns")]
    #[test_case(r#"craft.split("|x|y|z|", "|", true)"#, &["", "x", "y", "z", ""] ; "legacy_boolean_plain")]
    #[test_case(r#"craft.split("a1b22c333", "%d+")"#, &["a", "b", "c", ""] ; "pattern_separator")]
    #[test_case(r#"craft.split("abc", "")"#, &["a", "b", "c"] ; "empty_sep_splits_chars")]
    #[test_case(r#"craft.split("", ",")"#, &[""] ; "empty_string_one_piece")]
    #[test_case(r#"craft.split("", "")"#, &[] ; "empty_string_empty_sep")]
    #[test_case(r#"craft.split("a\nb", "\n")"#, &["a", "b"] ; "newline_no_trailing")]
    #[test_case(r#"craft.split("a\nb\n", "\n")"#, &["a", "b", ""] ; "newline_trailing_empty")]
    fn split_matches_vim_split(chunk: &str, expected: &[&str]) {
        assert_eq!(eval(chunk).unwrap(), expected);
    }

    #[test_case(r#"craft.split("ab", "x*")"#, INFINITE_LOOP_MSG ; "empty_match_pattern")]
    #[test_case(r#"craft.split("a", ",", 5)"#, OPTS_TYPE_MSG ; "bad_opts_type")]
    fn split_rejects(chunk: &str, expected_err: &str) {
        let err = eval(chunk).unwrap_err().to_string();
        assert!(err.contains(expected_err), "got: {err}");
    }
}
