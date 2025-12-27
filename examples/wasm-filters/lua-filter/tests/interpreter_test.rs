//! Integration tests for Lua interpreter

use lua_filter::lua::interpreter::{Interpreter, SharedState};
use lua_filter::lua::{lexer, parser};
use std::cell::RefCell;
use std::rc::Rc;

fn create_interpreter() -> Interpreter {
    let state = Rc::new(RefCell::new(SharedState::default()));
    Interpreter::with_state(state)
}

#[test]
fn test_basic_arithmetic() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize("return 1 + 2").unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(3.0));
}

#[test]
fn test_variable_assignment() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize("local x = 42; return x").unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_function_call() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function add(a, b) return a + b end; return add(10, 20)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(30.0));
}

#[test]
fn test_multiple_return_values() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() return 1, 2, 3 end; local a, b, c = f(); return a + b + c"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(6.0));
}

#[test]
fn test_pcall_success() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() return 42 end; local ok, result = pcall(f); return ok and result"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_pcall_error() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() error('test error') end; local ok, err = pcall(f); return ok"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), false);
}

#[test]
fn test_table_operations() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t = {}; t.key = 'value'; return t.key"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_lua_string(), "value");
}

#[test]
fn test_metatable_index() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t = {}; local mt = {__index = {default = 'default'}}; t = setmetatable(t, mt); return t.default"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_lua_string(), "default");
}

#[test]
fn test_string_operations() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "return string.upper('hello')"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_lua_string(), "HELLO");
}

#[test]
fn test_math_operations() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "return math.abs(-42)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_math_ult() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "return math.ult(1, 2)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_math_tointeger() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "return math.tointeger(42.0)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_table_move() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t1 = {10, 20, 30}; local t2 = {}; t2 = table.move(t1, 1, 3, 1, t2); return t2[1] + t2[2] + t2[3]"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(60.0));
}

#[test]
fn test_goto_label() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local x = 0; ::start:: x = x + 1; if x < 3 then goto start end; return x"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(3.0));
}

#[test]
fn test_string_pack() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local packed = string.pack('i4', 42); return #packed"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(4.0));
}

#[test]
fn test_string_unpack() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local packed = string.pack('i4', 42); local value = string.unpack('i4', packed); return value"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_rawget() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t = {}; local mt = {__index = {default = 'default'}}; setmetatable(t, mt); return rawget(t, 'key')"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert!(result.is_nil());
}

#[test]
fn test_rawset() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t = {}; local mt = {__newindex = function() error('should not be called') end}; setmetatable(t, mt); t = rawset(t, 'key', 'value'); return t.key"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_lua_string(), "value");
}

#[test]
fn test_math_randomseed() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "math.randomseed(42); local r1 = math.random(); math.randomseed(42); local r2 = math.random(); return r1 == r2"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_load() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local f, err = load('return 42'); if f then return f() else return err end"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_load_error() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local f, err = load('return +'); if f then return 'success' else return type(err) end"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_lua_string(), "string");
}

#[test]
fn test_require() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "package.loaded['test'] = {value = 42}; local m = require('test'); return m.value"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_string_dump() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function f() return 42 end; local dumped = string.dump(f); return type(dumped)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_lua_string(), "string");
}

#[test]
fn test_tail_call_optimization() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "function count(n) if n <= 0 then return 0 else return count(n - 1) + 1 end end; return count(1000)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(1000.0));
}

#[test]
fn test_string_gmatch() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local words = {}; for word in string.gmatch('hello world', '%w+') do table.insert(words, word) end; return #words"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(2.0));
}

#[test]
fn test_utf8_codes() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local count = 0; for code in utf8.codes('hello') do count = count + 1 end; return count"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(5.0));
}

#[test]
fn test_metatable_newindex() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t = {}; local calls = 0; local mt = {__newindex = function(t, k, v) calls = calls + 1 end}; t = setmetatable(t, mt); t.key = 'value'; return calls"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(1.0));
}

#[test]
fn test_metatable_call() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t = {}; local mt = {__call = function(self, x) return x * 2 end}; t = setmetatable(t, mt); return t(21)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_metatable_add() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t1 = {}; local t2 = {}; local mt = {__add = function(a, b) return 42 end}; t1 = setmetatable(t1, mt); return t1 + t2"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_metatable_eq() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t1 = {}; local t2 = {}; local mt = {__eq = function(a, b) return true end}; t1 = setmetatable(t1, mt); t2 = setmetatable(t2, mt); return t1 == t2"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.is_truthy(), true);
}

#[test]
fn test_metatable_len() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t = {}; local mt = {__len = function(self) return 42 end}; t = setmetatable(t, mt); return #t"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(42.0));
}

#[test]
fn test_metatable_tostring() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local t = {}; local mt = {__tostring = function(self) return 'custom' end}; t = setmetatable(t, mt); return tostring(t)"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_lua_string(), "custom");
}

#[test]
fn test_closure_upvalue() {
    let mut interpreter = create_interpreter();
    let tokens = lexer::tokenize(
        "local x = 10; function outer() local y = 20; return function() return x + y end end; local f = outer(); return f()"
    ).unwrap();
    let program = parser::parse(&tokens).unwrap();
    let result = interpreter.execute(&program).unwrap();
    assert_eq!(result.to_number(), Some(30.0));
}

