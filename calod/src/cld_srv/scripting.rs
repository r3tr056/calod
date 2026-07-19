//! Embedded Lua scripting (`EVAL`/`EVALSHA`/`SCRIPT`) via `mlua` (vendored Lua
//! 5.1, the same version Redis ships).
//!
//! # Re-entrancy model
//!
//! A Lua script can call back into the server with `redis.call(...)`. The store
//! dispatch (`ServedCalodStore::execute_inner`) is `async`, but the Lua VM is
//! driven synchronously. To bridge the two without blocking a runtime worker,
//! [`crate::cld_srv::served_store`] runs [`run_script`] on a `spawn_blocking`
//! thread and hands it a synchronous `call` closure. Each `redis.call` ships its
//! command over a channel to the async task (which awaits `execute_inner`) and
//! blocks on the reply. This module therefore stays free of any Tokio/store
//! dependencies — it only knows how to drive Lua and convert values to/from the
//! protocol [`Value`] type.
//!
//! # Value mapping (Redis Lua conventions)
//!
//! Lua → reply: `number`→integer (truncated), `string`→bulk, `true`→`:1`,
//! `false`/`nil`→nil, `{ok=…}`→status, `{err=…}`→error, array table→array.
//! reply → Lua (the `redis.call` result, always RESP2 wire types): integer→
//! number, bulk→string, status→`{ok=…}`, error→raised Lua error, array→table,
//! nil→`false`.

use mlua::{Lua, MultiValue, Value as LuaValue, Variadic};

use crate::parser::value::Value;

/// The synchronous callback `redis.call`/`redis.pcall` use to run a command.
/// Input is the full argument vector (`argv[0]` is the command name); output is
/// the reply [`Value`] (in RESP2 wire shape) or an error message string.
pub type CallFn<'a> = dyn Fn(Vec<Vec<u8>>) -> Result<Value, String> + 'a;

/// Compute the lowercase 40-char hex SHA1 of a script body (the `EVALSHA` key).
pub fn sha1hex(bytes: &[u8]) -> String {
    sha1_smol::Sha1::from(bytes).digest().to_string()
}

/// Run `source` with the given `KEYS`/`ARGV`, using `call` for `redis.call`.
/// Returns the script's return value mapped to a reply [`Value`], or a Lua/script
/// error message (without a leading `-`).
pub fn run_script(
    source: &str,
    keys: Vec<Vec<u8>>,
    argv: Vec<Vec<u8>>,
    call: &CallFn<'_>,
) -> Result<Value, String> {
    let lua = Lua::new();

    // KEYS / ARGV are plain (owned) string tables — set them before the scope.
    set_byte_table(&lua, "KEYS", &keys).map_err(lua_err_to_string)?;
    set_byte_table(&lua, "ARGV", &argv).map_err(lua_err_to_string)?;

    // `redis.call`/`pcall` borrow `call`, so they must be *scoped* functions
    // (mlua scopes permit non-'static closures that live only for the scope).
    lua.scope(|scope| {
        let redis = lua.create_table()?;

        let call_fn = scope.create_function(move |lua, args: Variadic<LuaValue>| {
            let argv = lua_args_to_bytes(&args)?;
            match call(argv) {
                Ok(v) => reply_to_lua(lua, v),
                Err(msg) => Err(mlua::Error::RuntimeError(msg)),
            }
        })?;
        redis.set("call", call_fn)?;

        let pcall_fn = scope.create_function(move |lua, args: Variadic<LuaValue>| {
            let argv = lua_args_to_bytes(&args)?;
            match call(argv) {
                Ok(v) => reply_to_lua(lua, v),
                Err(msg) => {
                    let t = lua.create_table()?;
                    t.set("err", msg)?;
                    Ok(LuaValue::Table(t))
                }
            }
        })?;
        redis.set("pcall", pcall_fn)?;

        redis.set(
            "error_reply",
            scope.create_function(|lua, msg: String| {
                let t = lua.create_table()?;
                t.set("err", msg)?;
                Ok(t)
            })?,
        )?;
        redis.set(
            "status_reply",
            scope.create_function(|lua, msg: String| {
                let t = lua.create_table()?;
                t.set("ok", msg)?;
                Ok(t)
            })?,
        )?;
        redis.set(
            "sha1hex",
            scope.create_function(|_, s: mlua::String| Ok(sha1hex(&s.as_bytes())))?,
        )?;
        // Accepted-but-ignored helpers so common scripts don't error out.
        redis.set(
            "log",
            scope.create_function(|_, _: Variadic<LuaValue>| Ok(()))?,
        )?;
        redis.set(
            "setresp",
            scope.create_function(|_, _: LuaValue| Ok(()))?,
        )?;
        redis.set(
            "replicate_commands",
            scope.create_function(|_, ()| Ok(true))?,
        )?;
        // Numeric log-level constants Redis exposes on the `redis` table.
        redis.set("LOG_DEBUG", 0)?;
        redis.set("LOG_VERBOSE", 1)?;
        redis.set("LOG_NOTICE", 2)?;
        redis.set("LOG_WARNING", 3)?;

        lua.globals().set("redis", &redis)?;
        // Redis also exposes the table under the `server` alias (7.x).
        lua.globals().set("server", redis)?;

        let result: MultiValue = lua.load(source).set_name("@user_script").eval()?;
        let first = result.into_iter().next().unwrap_or(LuaValue::Nil);
        Ok(lua_to_reply(&first))
    })
    .map_err(lua_err_to_string)
}

/// Build a 1-indexed Lua table of byte strings and bind it to a global name.
fn set_byte_table(lua: &Lua, name: &str, items: &[Vec<u8>]) -> mlua::Result<()> {
    let table = lua.create_table()?;
    for (i, item) in items.iter().enumerate() {
        table.set((i + 1) as i64, lua.create_string(item)?)?;
    }
    lua.globals().set(name, table)
}

/// Convert `redis.call` Lua arguments into raw byte argv (strings and numbers
/// only, matching Redis).
fn lua_args_to_bytes(args: &Variadic<LuaValue>) -> mlua::Result<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(args.len());
    for v in args.iter() {
        match v {
            LuaValue::String(s) => out.push(s.as_bytes().to_vec()),
            LuaValue::Integer(i) => out.push(i.to_string().into_bytes()),
            LuaValue::Number(n) => out.push(format_lua_number(*n).into_bytes()),
            _ => {
                return Err(mlua::Error::RuntimeError(
                    "Lua redis lib command arguments must be strings or integers".to_string(),
                ))
            }
        }
    }
    Ok(out)
}

/// Format a Lua float the way Redis does for command arguments.
fn format_lua_number(n: f64) -> String {
    if n == n.trunc() && n.is_finite() && n.abs() < 1e17 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Convert a `redis.call` reply [`Value`] (RESP2 wire shape) into a Lua value.
fn reply_to_lua(lua: &Lua, v: Value) -> mlua::Result<LuaValue> {
    Ok(match v {
        Value::Nil | Value::NilArray => LuaValue::Boolean(false),
        Value::Integer(i) => LuaValue::Integer(i),
        Value::SimpleString(s) => {
            let t = lua.create_table()?;
            t.set("ok", s)?;
            LuaValue::Table(t)
        }
        Value::Error(e) => {
            let t = lua.create_table()?;
            t.set("err", e)?;
            LuaValue::Table(t)
        }
        Value::BulkString(b) => LuaValue::String(lua.create_string(&b)?),
        Value::Array(items) | Value::Set(items) | Value::Push(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.into_iter().enumerate() {
                t.set((i + 1) as i64, reply_to_lua(lua, item)?)?;
            }
            LuaValue::Table(t)
        }
        Value::Map(pairs) => {
            // RESP2 scripts see a map as a flat array of alternating k, v.
            let t = lua.create_table()?;
            let mut idx = 1i64;
            for (k, val) in pairs {
                t.set(idx, reply_to_lua(lua, k)?)?;
                idx += 1;
                t.set(idx, reply_to_lua(lua, val)?)?;
                idx += 1;
            }
            LuaValue::Table(t)
        }
        Value::Double(d) => LuaValue::String(lua.create_string(format!("{d}"))?),
        Value::Boolean(b) => LuaValue::Integer(i64::from(b)),
        Value::BigNumber(s) => LuaValue::String(lua.create_string(s)?),
        Value::VerbatimString { data, .. } => LuaValue::String(lua.create_string(&data)?),
    })
}

/// Convert a script's Lua return value into a reply [`Value`].
fn lua_to_reply(v: &LuaValue) -> Value {
    match v {
        LuaValue::Nil => Value::Nil,
        LuaValue::Boolean(false) => Value::Nil,
        LuaValue::Boolean(true) => Value::Integer(1),
        LuaValue::Integer(i) => Value::Integer(*i),
        LuaValue::Number(n) => Value::Integer(*n as i64),
        LuaValue::String(s) => Value::BulkString(s.as_bytes().to_vec()),
        LuaValue::Table(t) => {
            // `{err = "..."}` and `{ok = "..."}` are special status/error forms.
            if let Ok(LuaValue::String(e)) = t.get::<LuaValue>("err") {
                return Value::Error(String::from_utf8_lossy(&e.as_bytes()).into_owned());
            }
            if let Ok(LuaValue::String(ok)) = t.get::<LuaValue>("ok") {
                return Value::SimpleString(String::from_utf8_lossy(&ok.as_bytes()).into_owned());
            }
            // Otherwise an array: contiguous 1.. until the first nil.
            let mut items = Vec::new();
            let mut i = 1i64;
            loop {
                match t.raw_get::<LuaValue>(i) {
                    Ok(LuaValue::Nil) | Err(_) => break,
                    Ok(elem) => items.push(lua_to_reply(&elem)),
                }
                i += 1;
            }
            Value::Array(items)
        }
        // Functions/userdata/threads are not representable → nil, like Redis.
        _ => Value::Nil,
    }
}

/// Reduce an mlua error to a single human-readable line for the error reply.
fn lua_err_to_string(e: mlua::Error) -> String {
    let full = match &e {
        mlua::Error::RuntimeError(m) | mlua::Error::SyntaxError { message: m, .. } => m.clone(),
        mlua::Error::CallbackError { cause, .. } => {
            // Unwrap to the originating runtime error message when possible.
            if let mlua::Error::RuntimeError(m) = cause.as_ref() {
                m.clone()
            } else {
                cause.to_string()
            }
        }
        other => other.to_string(),
    };
    full.lines().next().unwrap_or("script error").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_call(_: Vec<Vec<u8>>) -> Result<Value, String> {
        Err("ERR no redis.call in this test".to_string())
    }

    #[test]
    fn returns_number_truncated() {
        let v = run_script("return 1 + 2", vec![], vec![], &no_call).unwrap();
        assert_eq!(v, Value::Integer(3));
        let v = run_script("return 3.9", vec![], vec![], &no_call).unwrap();
        assert_eq!(v, Value::Integer(3));
    }

    #[test]
    fn returns_string_and_array() {
        assert_eq!(
            run_script("return 'hello'", vec![], vec![], &no_call).unwrap(),
            Value::BulkString(b"hello".to_vec())
        );
        assert_eq!(
            run_script("return {1, 2, 3}", vec![], vec![], &no_call).unwrap(),
            Value::Array(vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3)
            ])
        );
    }

    #[test]
    fn exposes_keys_and_argv() {
        let v = run_script(
            "return {KEYS[1], ARGV[1], ARGV[2]}",
            vec![b"k1".to_vec()],
            vec![b"a1".to_vec(), b"a2".to_vec()],
            &no_call,
        )
        .unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                Value::BulkString(b"k1".to_vec()),
                Value::BulkString(b"a1".to_vec()),
                Value::BulkString(b"a2".to_vec()),
            ])
        );
    }

    #[test]
    fn status_and_error_replies() {
        assert_eq!(
            run_script("return redis.status_reply('OK')", vec![], vec![], &no_call).unwrap(),
            Value::SimpleString("OK".to_string())
        );
        assert_eq!(
            run_script("return redis.error_reply('boom')", vec![], vec![], &no_call).unwrap(),
            Value::Error("boom".to_string())
        );
        assert_eq!(
            run_script("return {err='myerr'}", vec![], vec![], &no_call).unwrap(),
            Value::Error("myerr".to_string())
        );
    }

    #[test]
    fn false_is_nil_true_is_one() {
        assert_eq!(
            run_script("return false", vec![], vec![], &no_call).unwrap(),
            Value::Nil
        );
        assert_eq!(
            run_script("return true", vec![], vec![], &no_call).unwrap(),
            Value::Integer(1)
        );
        assert_eq!(
            run_script("return nil", vec![], vec![], &no_call).unwrap(),
            Value::Nil
        );
    }

    #[test]
    fn redis_call_roundtrips_via_stub() {
        let call = |argv: Vec<Vec<u8>>| -> Result<Value, String> {
            assert_eq!(argv[0], b"GET");
            assert_eq!(argv[1], b"k1");
            Ok(Value::BulkString(b"v".to_vec()))
        };
        let v = run_script(
            "return redis.call('GET', KEYS[1])",
            vec![b"k1".to_vec()],
            vec![],
            &call,
        )
        .unwrap();
        assert_eq!(v, Value::BulkString(b"v".to_vec()));
    }

    #[test]
    fn pcall_catches_error() {
        let call = |_: Vec<Vec<u8>>| -> Result<Value, String> { Err("WRONGTYPE bad".to_string()) };
        let v = run_script(
            "local r = redis.pcall('GET','k'); if r.err then return 'caught' else return 'no' end",
            vec![],
            vec![],
            &call,
        )
        .unwrap();
        assert_eq!(v, Value::BulkString(b"caught".to_vec()));
    }

    #[test]
    fn call_error_aborts_script() {
        let call = |_: Vec<Vec<u8>>| -> Result<Value, String> { Err("WRONGTYPE bad".to_string()) };
        let err = run_script("return redis.call('GET','k')", vec![], vec![], &call).unwrap_err();
        assert!(err.contains("WRONGTYPE"), "got: {err}");
    }

    #[test]
    fn syntax_error_is_reported() {
        assert!(run_script("this is not valid lua", vec![], vec![], &no_call).is_err());
    }

    #[test]
    fn sha1hex_is_40_hex_chars() {
        let h = sha1hex(b"return 1");
        assert_eq!(h.len(), 40);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Deterministic.
        assert_eq!(h, sha1hex(b"return 1"));
    }
}
