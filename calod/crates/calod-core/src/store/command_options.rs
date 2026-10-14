//! Command option enums shared between the command parser (calod-resp) and
//! the storage engine. They live in calod-core so the store can depend on
//! them without a crate cycle; `cld_srv::command` re-exports them.

#[derive(Debug, PartialEq, Clone)]
pub enum SetExpireOption {
    EX(u64), // EX seconds
    PX(u64), // PX milliseconds
}

#[derive(Debug, PartialEq, Clone)]
pub enum SetOption {
    NX, // NX -- Only set the key if it does not already exist.
    XX, // XX -- Only set the key if it already exist.
}

#[derive(Debug, PartialEq, Clone)]
pub enum GetExOption {
    EX(u64),
    PX(u64),
    EXAT(u64),
    PXAT(u64),
    Persist,
}

#[derive(Debug, PartialEq, Clone)]
pub enum InsertOption {
    Before,
    After,
}
