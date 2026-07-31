use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mlua::{AnyUserData, Error, MultiValue, UserData, UserDataMethods, Value};
use tree_sitter::{Node, Point, Tree};

use super::tree::LuaTree;

const NODE_NOT_FOUND_MSG: &str = "node not found in tree";

#[derive(Clone)]
pub(crate) struct LuaNode {
    pub(crate) tree: Arc<Tree>,
    start: usize,
    end: usize,
    id: usize,
}

impl LuaNode {
    pub(crate) fn new(node: Node<'_>, tree: Arc<Tree>) -> Self {
        Self {
            tree,
            start: node.start_byte(),
            end: node.end_byte(),
            id: node.id(),
        }
    }

    pub(crate) fn ts_node(&self) -> mlua::Result<Node<'_>> {
        let root = self.tree.root_node();

        // Any node spanning exactly [start, end] is an ancestor of the
        // smallest node containing that range, so climbing resolves every
        // node the descendant lookup can see.
        if let Some(mut node) = root.descendant_for_byte_range(self.start, self.end) {
            loop {
                if node.id() == self.id {
                    return Ok(node);
                }
                match node.parent() {
                    Some(parent) => node = parent,
                    None => break,
                }
            }
        }

        // Zero-width nodes (e.g. MISSING ones) are invisible to
        // `descendant_for_byte_range`; descend only through nodes whose
        // range contains ours, so this stays O(depth), not a tree walk.
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.id() == self.id {
                return Ok(node);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.start_byte() <= self.start && child.end_byte() >= self.end {
                    stack.push(child);
                }
            }
        }

        Err(Error::runtime(NODE_NOT_FOUND_MSG))
    }

    fn wrap(&self, node: Node) -> Self {
        Self::new(node, Arc::clone(&self.tree))
    }

    fn wrap_opt(&self, node: Option<Node>) -> Option<Self> {
        node.map(|n| self.wrap(n))
    }
}

impl UserData for LuaNode {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("type", |_, this, ()| Ok(this.ts_node()?.kind().to_owned()));

        methods.add_method("symbol", |_, this, ()| Ok(this.ts_node()?.kind_id() as i64));

        methods.add_method("id", |_, this, ()| Ok(format!("{}", this.ts_node()?.id())));

        methods.add_method("range", |_, this, include_bytes: Option<bool>| {
            let node = this.ts_node()?;
            let sp = node.start_position();
            let ep = node.end_position();
            if include_bytes.unwrap_or(false) {
                Ok(MultiValue::from_iter([
                    Value::Integer(sp.row as i64),
                    Value::Integer(sp.column as i64),
                    Value::Integer(node.start_byte() as i64),
                    Value::Integer(ep.row as i64),
                    Value::Integer(ep.column as i64),
                    Value::Integer(node.end_byte() as i64),
                ]))
            } else {
                Ok(MultiValue::from_iter([
                    Value::Integer(sp.row as i64),
                    Value::Integer(sp.column as i64),
                    Value::Integer(ep.row as i64),
                    Value::Integer(ep.column as i64),
                ]))
            }
        });

        methods.add_method("start", |_, this, ()| {
            let node = this.ts_node()?;
            let sp = node.start_position();
            Ok((sp.row as i64, sp.column as i64, node.start_byte() as i64))
        });

        methods.add_method("end_", |_, this, ()| {
            let node = this.ts_node()?;
            let ep = node.end_position();
            Ok((ep.row as i64, ep.column as i64, node.end_byte() as i64))
        });

        methods.add_method("byte_length", |_, this, ()| {
            let node = this.ts_node()?;
            Ok((node.end_byte() - node.start_byte()) as i64)
        });

        methods.add_method("child", |_, this, index: u32| {
            Ok(this.wrap_opt(this.ts_node()?.child(index)))
        });

        methods.add_method("named_child", |_, this, index: u32| {
            Ok(this.wrap_opt(this.ts_node()?.named_child(index)))
        });

        methods.add_method("child_count", |_, this, ()| {
            Ok(this.ts_node()?.child_count() as i64)
        });

        methods.add_method("named_child_count", |_, this, ()| {
            Ok(this.ts_node()?.named_child_count() as i64)
        });

        methods.add_method("children", |lua, this, ()| {
            let tbl = lua.create_table()?;
            let node = this.ts_node()?;
            let mut cursor = node.walk();
            for (i, child) in node.children(&mut cursor).enumerate() {
                tbl.raw_set(i + 1, this.wrap(child))?;
            }
            Ok(tbl)
        });

        methods.add_method("named_children", |lua, this, ()| {
            let tbl = lua.create_table()?;
            let node = this.ts_node()?;
            let mut cursor = node.walk();
            for (i, child) in node.named_children(&mut cursor).enumerate() {
                tbl.raw_set(i + 1, this.wrap(child))?;
            }
            Ok(tbl)
        });

        methods.add_method("iter_children", |lua, this, ()| {
            let node = this.ts_node()?;
            let count = node.child_count() as u32;
            let mut entries: Vec<(LuaNode, Option<String>)> = Vec::with_capacity(count as usize);
            for i in 0..count {
                if let Some(child) = node.child(i) {
                    let field = node.field_name_for_child(i).map(str::to_owned);
                    entries.push((this.wrap(child), field));
                }
            }
            let idx = Arc::new(AtomicUsize::new(0));
            let entries = Arc::new(entries);
            lua.create_function(move |lua, ()| {
                let i = idx.fetch_add(1, Ordering::Relaxed);
                if i >= entries.len() {
                    return Ok(MultiValue::new());
                }
                let (ref lua_node, ref field) = entries[i];
                let child = lua_node.clone();
                Ok(MultiValue::from_iter([
                    Value::UserData(lua.create_userdata(child)?),
                    match field {
                        Some(s) => Value::String(lua.create_string(s)?),
                        None => Value::Nil,
                    },
                ]))
            })
        });

        methods.add_method("field", |lua, this, name: String| {
            let tbl = lua.create_table()?;
            let node = this.ts_node()?;
            let mut cursor = node.walk();
            for (i, child) in node.children_by_field_name(&name, &mut cursor).enumerate() {
                tbl.raw_set(i + 1, this.wrap(child))?;
            }
            Ok(tbl)
        });

        methods.add_method("parent", |_, this, ()| {
            Ok(this.wrap_opt(this.ts_node()?.parent()))
        });

        methods.add_method("next_sibling", |_, this, ()| {
            Ok(this.wrap_opt(this.ts_node()?.next_sibling()))
        });

        methods.add_method("prev_sibling", |_, this, ()| {
            Ok(this.wrap_opt(this.ts_node()?.prev_sibling()))
        });

        methods.add_method("next_named_sibling", |_, this, ()| {
            Ok(this.wrap_opt(this.ts_node()?.next_named_sibling()))
        });

        methods.add_method("prev_named_sibling", |_, this, ()| {
            Ok(this.wrap_opt(this.ts_node()?.prev_named_sibling()))
        });

        methods.add_method("child_with_descendant", |_, this, desc: AnyUserData| {
            let desc = desc.borrow::<LuaNode>()?;
            Ok(this.wrap_opt(this.ts_node()?.child_with_descendant(desc.ts_node()?)))
        });

        methods.add_method(
            "descendant_for_range",
            |_, this, (sr, sc, er, ec): (usize, usize, usize, usize)| {
                let start = Point::new(sr, sc);
                let end = Point::new(er, ec);
                Ok(this.wrap_opt(this.ts_node()?.descendant_for_point_range(start, end)))
            },
        );

        methods.add_method(
            "named_descendant_for_range",
            |_, this, (sr, sc, er, ec): (usize, usize, usize, usize)| {
                let start = Point::new(sr, sc);
                let end = Point::new(er, ec);
                Ok(this.wrap_opt(this.ts_node()?.named_descendant_for_point_range(start, end)))
            },
        );

        methods.add_method("named", |_, this, ()| Ok(this.ts_node()?.is_named()));

        methods.add_method("extra", |_, this, ()| Ok(this.ts_node()?.is_extra()));

        methods.add_method("missing", |_, this, ()| Ok(this.ts_node()?.is_missing()));

        methods.add_method("has_error", |_, this, ()| Ok(this.ts_node()?.has_error()));

        methods.add_method("has_changes", |_, this, ()| {
            Ok(this.ts_node()?.has_changes())
        });

        methods.add_method("equal", |_, this, other: AnyUserData| {
            let other = other.borrow::<LuaNode>()?;
            Ok(this.id == other.id)
        });

        methods.add_method("sexpr", |_, this, ()| Ok(this.ts_node()?.to_sexp()));

        methods.add_method("tree", |_, this, ()| {
            Ok(LuaTree {
                inner: Arc::clone(&this.tree),
            })
        });
    }
}
