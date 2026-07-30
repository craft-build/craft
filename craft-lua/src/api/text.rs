use mlua::{Lua, Result as LuaResult, Table};

use super::util::pair::pair;

pub(crate) fn create_text_table(lua: &Lua) -> LuaResult<Table> {
    let text = lua.create_table()?;

    text.set(
        "html_to_markdown",
        lua.create_function(|_lua, html: String| {
            Ok(pair(
                htmd::convert(&html).map_err(|e| format!("html_to_markdown: {e}")),
            ))
        })?,
    )?;

    Ok(text)
}
