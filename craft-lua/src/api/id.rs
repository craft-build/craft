//! `craft.id`: `new()` returns a globally unique identifier.

use mlua::{Lua, Result as LuaResult, Table};

pub(crate) fn create_id_table(lua: &Lua) -> LuaResult<Table> {
    let t = lua.create_table()?;

    t.set(
        "new",
        lua.create_function(|_, ()| Ok(craft_storage::id::CraftId::generate().to_string()))?,
    )?;

    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_returns_unique_ids() {
        let lua = Lua::new();
        let tbl = create_id_table(&lua).unwrap();
        let new: mlua::Function = tbl.get("new").unwrap();

        let first = new.call::<String>(()).unwrap();
        let second = new.call::<String>(()).unwrap();

        assert_ne!(first, second);
        assert!(first.parse::<craft_storage::id::CraftId>().is_ok());
        assert!(second.parse::<craft_storage::id::CraftId>().is_ok());
    }
}
