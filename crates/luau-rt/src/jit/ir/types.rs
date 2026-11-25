//! IR value types.
//!
//! A type here is what the trace *knows* about a value after its guards:
//! a value loaded from the VM stack starts as whatever its type guard
//! checked; arithmetic on numbers produces numbers, and so on. `Any` is a
//! boxed TValue whose tag isn't known (only moved around, never computed on).

use crate::lobject::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IrType {
    Nil,
    Boolean,
    /// f64
    Number,
    /// Luau 64-bit integer (`LUA_TINTEGER`)
    Integer,
    /// 3 x f32
    Vector,
    LightUserdata,
    String,
    Table,
    Function,
    Userdata,
    Buffer,
    Thread,
    /// raw pointer into VM memory (a TValue slot, an array part, ...):
    /// never stored to the Lua stack
    Ptr,
    /// a 32-bit machine integer (array indices, lengths): never stored
    I32,
    /// a 64-bit machine integer (integer-valued numbers computed exactly,
    /// see `optimizer::narrow::int_mod_phi`): never stored
    I64,
    /// a boxed TValue of unknown type
    Any,
}

impl IrType {
    /// The VM tag for values of this type, if it's a Lua type.
    pub fn tag(self) -> Option<i32> {
        Some(match self {
            IrType::Nil => LUA_TNIL,
            IrType::Boolean => LUA_TBOOLEAN,
            IrType::Number => LUA_TNUMBER,
            IrType::Integer => LUA_TINTEGER,
            IrType::Vector => LUA_TVECTOR,
            IrType::LightUserdata => LUA_TLIGHTUSERDATA,
            IrType::String => LUA_TSTRING,
            IrType::Table => LUA_TTABLE,
            IrType::Function => LUA_TFUNCTION,
            IrType::Userdata => LUA_TUSERDATA,
            IrType::Buffer => LUA_TBUFFER,
            IrType::Thread => LUA_TTHREAD,
            IrType::Ptr | IrType::I32 | IrType::I64 | IrType::Any => return None,
        })
    }

    /// The type a VM tag guards for (`None` for tags traces don't handle).
    pub fn of_tag(tt: i32) -> Option<IrType> {
        Some(match tt {
            LUA_TNIL => IrType::Nil,
            LUA_TBOOLEAN => IrType::Boolean,
            LUA_TNUMBER => IrType::Number,
            LUA_TINTEGER => IrType::Integer,
            LUA_TVECTOR => IrType::Vector,
            LUA_TLIGHTUSERDATA => IrType::LightUserdata,
            LUA_TSTRING => IrType::String,
            LUA_TTABLE => IrType::Table,
            LUA_TFUNCTION => IrType::Function,
            LUA_TUSERDATA => IrType::Userdata,
            LUA_TBUFFER => IrType::Buffer,
            LUA_TTHREAD => IrType::Thread,
            _ => return None,
        })
    }

    /// Lua values that live in a GC object (a pointer payload).
    pub fn is_gc(self) -> bool {
        matches!(
            self,
            IrType::String | IrType::Table | IrType::Function | IrType::Userdata | IrType::Buffer | IrType::Thread
        )
    }

    /// Values kept in an xmm register when unboxed.
    pub fn is_fp(self) -> bool {
        matches!(self, IrType::Number | IrType::Vector)
    }

    /// Types that may be written back to a Lua stack slot.
    pub fn is_lua(self) -> bool {
        self.tag().is_some() || self == IrType::Any
    }
}
