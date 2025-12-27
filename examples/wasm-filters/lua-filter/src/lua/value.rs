//! Lua Value Types

use crate::lua::ast::Stmt;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

/// Captured upvalue for closures
#[derive(Clone, Debug)]
pub struct Upvalue {
    pub value: Rc<RefCell<LuaValue>>,
}

impl Upvalue {
    pub fn new(value: LuaValue) -> Self {
        Self {
            value: Rc::new(RefCell::new(value)),
        }
    }

    /// Get the current value of this upvalue
    ///
    /// Returns a clone of the current value stored in this upvalue.
    #[allow(dead_code)] // Public API for future use
    pub fn get(&self) -> LuaValue {
        self.value.borrow().clone()
    }

    pub fn set(&self, value: LuaValue) {
        *self.value.borrow_mut() = value;
    }
}

/// Metatable for table metamethods
#[derive(Clone, Debug, Default)]
pub struct Metatable {
    pub index: Option<Box<LuaValue>>,
    pub newindex: Option<Box<LuaValue>>,
    pub call: Option<Box<LuaValue>>,
    pub tostring: Option<Box<LuaValue>>,
    pub add: Option<Box<LuaValue>>,
    pub sub: Option<Box<LuaValue>>,
    pub mul: Option<Box<LuaValue>>,
    pub div: Option<Box<LuaValue>>,
    pub mod_: Option<Box<LuaValue>>,
    pub pow: Option<Box<LuaValue>>,
    pub unm: Option<Box<LuaValue>>,
    pub eq: Option<Box<LuaValue>>,
    pub lt: Option<Box<LuaValue>>,
    pub le: Option<Box<LuaValue>>,
    pub len: Option<Box<LuaValue>>,
    pub concat: Option<Box<LuaValue>>,
}

/// Lua table with optional metatable
#[derive(Clone, Debug)]
pub struct LuaTable {
    pub data: HashMap<String, LuaValue>,
    pub metatable: Option<Box<Metatable>>,
}

impl LuaTable {
    pub fn new() -> Self {
        Self {
            data: HashMap::new(),
            metatable: None,
        }
    }

    pub fn from_map(data: HashMap<String, LuaValue>) -> Self {
        Self {
            data,
            metatable: None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&LuaValue> {
        self.data.get(key)
    }

    pub fn set(&mut self, key: String, value: LuaValue) {
        self.data.insert(key, value);
    }

    pub fn len(&self) -> usize {
        // Array length: highest consecutive integer key starting from 1
        let mut len = 0;
        loop {
            if self.data.contains_key(&(len + 1).to_string()) {
                len += 1;
            } else {
                break;
            }
        }
        len
    }
}

impl Default for LuaTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Closure: function with captured environment
#[derive(Clone, Debug)]
pub struct Closure {
    pub name: Option<String>,
    pub params: Vec<String>,
    pub vararg: bool,
    pub body: Vec<Stmt>,
    pub upvalues: HashMap<String, Upvalue>,
}

impl Closure {
    pub fn new(
        name: Option<String>,
        params: Vec<String>,
        vararg: bool,
        body: Vec<Stmt>,
        upvalues: HashMap<String, Upvalue>,
    ) -> Self {
        Self {
            name,
            params,
            vararg,
            body,
            upvalues,
        }
    }
}

/// Coroutine status
#[derive(Clone, Debug, PartialEq)]
pub enum CoroutineStatus {
    Suspended,  // yieldで一時停止
    Running,    // 実行中
    Dead,       // 終了
}

/// Scope snapshot for coroutine state
#[derive(Clone, Debug)]
pub struct ScopeSnapshot {
    pub values: HashMap<String, Rc<RefCell<LuaValue>>>,
}

/// Coroutine state
#[derive(Clone, Debug)]
pub struct CoroutineState {
    /// 実行中のクロージャ
    pub closure: Rc<Closure>,
    
    /// スコープスタックの状態（コルーチン実行時のスナップショット）
    pub scopes: Vec<ScopeSnapshot>,
    
    /// 現在のステートメントインデックス（closure.body内の位置）
    pub statement_index: usize,
    
    /// 実行状態フラグ
    pub break_flag: bool,
    pub return_values: Option<Vec<LuaValue>>,
    pub varargs: Vec<LuaValue>,
    
    /// コルーチンの状態
    pub status: CoroutineStatus,
    
    /// 呼び出し元に返す値（yield時の値）
    pub yield_values: Option<Vec<LuaValue>>,
}

/// Lua value types
#[derive(Clone, Debug)]
pub enum LuaValue {
    Nil,
    Boolean(bool),
    Number(f64),
    String(String),
    Table(LuaTable),
    #[allow(dead_code)] // Reserved for future use
    Function(String), // Named function reference
    Closure(Rc<Closure>), // Closure with captured environment
    NativeFunction(String), // Built-in function name
    Coroutine(Rc<RefCell<CoroutineState>>), // Coroutine state
}

impl Default for LuaValue {
    fn default() -> Self {
        LuaValue::Nil
    }
}

impl LuaValue {
    pub fn is_truthy(&self) -> bool {
        match self {
            LuaValue::Nil => false,
            LuaValue::Boolean(b) => *b,
            _ => true,
        }
    }

    pub fn is_nil(&self) -> bool {
        matches!(self, LuaValue::Nil)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            LuaValue::Nil => "nil",
            LuaValue::Boolean(_) => "boolean",
            LuaValue::Number(_) => "number",
            LuaValue::String(_) => "string",
            LuaValue::Table(_) => "table",
            LuaValue::Function(_) | LuaValue::Closure(_) | LuaValue::NativeFunction(_) => "function",
            LuaValue::Coroutine(_) => "thread",
        }
    }

    pub fn to_number(&self) -> Option<f64> {
        match self {
            LuaValue::Number(n) => Some(*n),
            LuaValue::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn to_lua_string(&self) -> String {
        match self {
            LuaValue::Nil => "nil".to_string(),
            LuaValue::Boolean(b) => b.to_string(),
            LuaValue::Number(n) => {
                if n.fract() == 0.0 {
                    format!("{}", *n as i64)
                } else {
                    n.to_string()
                }
            }
            LuaValue::String(s) => s.clone(),
            LuaValue::Table(_) => "table".to_string(),
            LuaValue::Function(name) => format!("function: {}", name),
            LuaValue::Closure(c) => {
                if let Some(name) = &c.name {
                    format!("function: {}", name)
                } else {
                    "function: (anonymous)".to_string()
                }
            }
            LuaValue::NativeFunction(name) => format!("function: {}", name),
            LuaValue::Coroutine(co) => {
                let status = co.borrow().status.clone();
                match status {
                    CoroutineStatus::Suspended => "thread: suspended".to_string(),
                    CoroutineStatus::Running => "thread: running".to_string(),
                    CoroutineStatus::Dead => "thread: dead".to_string(),
                }
            },
        }
    }

    /// Create a table from a HashMap (convenience)
    pub fn table(data: HashMap<String, LuaValue>) -> Self {
        LuaValue::Table(LuaTable::from_map(data))
    }

    /// Get table data if this is a table
    ///
    /// Returns a reference to the table if this value is a table, otherwise returns None.
    #[allow(dead_code)] // Public API for future use
    pub fn as_table(&self) -> Option<&LuaTable> {
        match self {
            LuaValue::Table(t) => Some(t),
            _ => None,
        }
    }

    /// Get mutable table data if this is a table
    ///
    /// Returns a mutable reference to the table if this value is a table, otherwise returns None.
    #[allow(dead_code)] // Public API for future use
    pub fn as_table_mut(&mut self) -> Option<&mut LuaTable> {
        match self {
            LuaValue::Table(t) => Some(t),
            _ => None,
        }
    }
}

impl fmt::Display for LuaValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_lua_string())
    }
}

impl PartialEq for LuaValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (LuaValue::Nil, LuaValue::Nil) => true,
            (LuaValue::Boolean(a), LuaValue::Boolean(b)) => a == b,
            (LuaValue::Number(a), LuaValue::Number(b)) => (a - b).abs() < f64::EPSILON,
            (LuaValue::String(a), LuaValue::String(b)) => a == b,
            _ => false,
        }
    }
}

// MultiValue struct removed - multiple return values are now handled directly as Vec<LuaValue>
