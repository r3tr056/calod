use serde_json;
use std::convert::TryFrom;
use thiserror::Error;
use tracing::error;

use crate::parser::value::Value;

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("Invalid command format")]
    InvalidFormat,
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
    #[error("JSON parse error: {0}")]
    JsonError(#[from] serde_json::Error),
}

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

#[derive(Debug, Clone, PartialEq)]
pub enum ClientSubCommand {
    List,
    Info,
    GetName,
    SetName { connection_name: String },
    Kill { ip_port: String }, // or ClientKillFilter, for more complex KILL options
    Id,                       // CLIENT ID -> integer connection id
    SetInfo,                  // CLIENT SETINFO LIB-NAME|LIB-VER <val> -> OK
    NoEvict,                  // CLIENT NO-EVICT on|off -> OK
    NoTouch,                  // CLIENT NO-TOUCH on|off -> OK
    Unpause,                  // CLIENT UNPAUSE -> OK
    NoSubCommand,             // Represent no subcommand or invalid subcommand for CLIENT
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigSubCommand {
    Get { params: Vec<String> },
    Set { pairs: Vec<(String, String)> },
    ResetStat,
    Rewrite,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScriptSubCommand {
    /// `SCRIPT LOAD <script>` — cache the body, return its SHA1.
    Load { source: String },
    /// `SCRIPT EXISTS <sha1> [<sha1> ...]` — 0/1 per requested digest.
    Exists { shas: Vec<String> },
    /// `SCRIPT FLUSH [ASYNC|SYNC]` — drop all cached scripts.
    Flush,
    /// Any other/unsupported subcommand (answered with OK where harmless).
    NoOp,
}

#[derive(Debug, PartialEq, Clone)]
pub enum Command {
    // Connection commands
    // Generic
    Ping {},
    Quit {},
    Hello {
        protover: Option<u8>,
        auth: Option<(String, String)>,
        setname: Option<String>,
    },
    Echo {
        message: String,
    },
    CommandInfo {
        command_name: String,
    },
    CommandList {},
    CommandHelp {
        command_name: String,
    },
    Command {},
    Auth {
        password: String,
    },
    Client {
        subcommand: ClientSubCommand,
    },
    Config {
        sub: ConfigSubCommand,
    },
    CommandCount {},
    CommandDocs {
        command_names: Vec<String>,
    },
    Info {
        section: Option<String>,
    },

    // Scripting commands (EVAL_RO/EVALSHA_RO map onto Eval/EvalSha; we do not
    // enforce read-only). KEYS/ARGV are binary-safe.
    Eval {
        source: String,
        numkeys: usize,
        keys: Vec<Vec<u8>>,
        args: Vec<Vec<u8>>,
    },
    EvalSha {
        sha: String,
        numkeys: usize,
        keys: Vec<Vec<u8>>,
        args: Vec<Vec<u8>>,
    },
    Script {
        sub: ScriptSubCommand,
    },

    // Admin / introspection (Phase 13). Most are thin, well-formed replies so
    // clients that probe them on connect/health-check succeed.
    /// `DEBUG <subcommand> [args...]` (JMAP/SLEEP/OBJECT/SET-ACTIVE-EXPIRE/RELOAD/…).
    Debug {
        args: Vec<String>,
    },
    /// `RESET` — reset the connection to its post-handshake default state.
    Reset {},
    /// `SLOWLOG GET|RESET|LEN [arg]`.
    SlowLog {
        sub: String,
        arg: Option<String>,
    },
    /// `MEMORY USAGE <key> | DOCTOR | STATS | MALLOC-STATS | PURGE`.
    Memory {
        sub: String,
        key: Option<String>,
    },
    /// `LATENCY HISTORY|RESET|LATEST|DOCTOR [args]`.
    Latency {
        sub: String,
    },
    /// `LOLWUT [VERSION n]`.
    Lolwut {},
    /// `ACL <subcommand> [args...]` (WHOAMI/LIST/CAT/GETUSER/…).
    Acl {
        args: Vec<String>,
    },
    /// `FUNCTION <subcommand> [args...]` (LIST/DUMP/STATS/FLUSH/…).
    Function {
        args: Vec<String>,
    },
    /// `WAITAOF numlocal numreplicas timeout`.
    WaitAof {
        numlocal: i64,
        numreplicas: i64,
        timeout: i64,
    },
    /// `FAILOVER [ABORT|...]`.
    Failover {},
    /// `SAVE` — synchronously snapshot the dataset to disk.
    Save {},
    /// `BGSAVE [SCHEDULE]` — snapshot in the background.
    BgSave {},
    /// `LASTSAVE` — unix time of the last successful save.
    LastSave {},
    /// `BGREWRITEAOF` — rewrite/compact the append-only file in the background.
    BgRewriteAof {},

    // Pub/Sub commands
    Subscribe {
        channels: Vec<String>,
    },
    Unsubscribe {
        channels: Vec<String>, // empty = unsubscribe from all
    },
    PSubscribe {
        patterns: Vec<String>,
    },
    PUnsubscribe {
        patterns: Vec<String>, // empty = unsubscribe from all patterns
    },
    Publish {
        channel: String,
        message: Vec<u8>,
    },
    PubSubChannels {
        pattern: Option<String>,
    },
    PubSubNumSub {
        channels: Vec<String>,
    },
    PubSubNumPat {},

    // Transaction commands
    Multi {},
    Exec {},
    Discard {},
    Watch {
        keys: Vec<String>,
    },
    Unwatch {},

    // Sorted set commands
    ZAdd {
        key: String,
        nx: bool,
        xx: bool,
        gt: bool,
        lt: bool,
        ch: bool,
        incr: bool,
        members: Vec<(f64, Vec<u8>)>,
    },
    ZScore {
        key: String,
        member: Vec<u8>,
    },
    ZMScore {
        key: String,
        members: Vec<Vec<u8>>,
    },
    ZCard {
        key: String,
    },
    ZIncrBy {
        key: String,
        increment: f64,
        member: Vec<u8>,
    },
    ZRem {
        key: String,
        members: Vec<Vec<u8>>,
    },
    ZRank {
        key: String,
        member: Vec<u8>,
        with_score: bool,
    },
    ZRevRank {
        key: String,
        member: Vec<u8>,
        with_score: bool,
    },
    ZRange {
        key: String,
        start: i64,
        stop: i64,
        with_scores: bool,
    },
    ZRevRange {
        key: String,
        start: i64,
        stop: i64,
        with_scores: bool,
    },
    ZRangeByScore {
        key: String,
        min: String,
        max: String,
        with_scores: bool,
        limit: Option<(i64, i64)>,
    },
    ZRevRangeByScore {
        key: String,
        max: String,
        min: String,
        with_scores: bool,
        limit: Option<(i64, i64)>,
    },
    ZRangeByLex {
        key: String,
        min: String,
        max: String,
        limit: Option<(i64, i64)>,
    },
    ZRevRangeByLex {
        key: String,
        max: String,
        min: String,
        limit: Option<(i64, i64)>,
    },
    ZCount {
        key: String,
        min: String,
        max: String,
    },
    ZLexCount {
        key: String,
        min: String,
        max: String,
    },
    ZPopMin {
        key: String,
        count: Option<usize>,
    },
    ZPopMax {
        key: String,
        count: Option<usize>,
    },
    ZRemRangeByRank {
        key: String,
        start: i64,
        stop: i64,
    },
    ZRemRangeByScore {
        key: String,
        min: String,
        max: String,
    },
    ZRemRangeByLex {
        key: String,
        min: String,
        max: String,
    },
    ZRandMember {
        key: String,
        count: Option<i64>,
        with_scores: bool,
    },
    ZScan {
        key: String,
        cursor: u64,
        pattern: Option<String>,
        count: Option<usize>,
    },

    // Bitmap & HyperLogLog commands
    SetBit {
        key: String,
        offset: u64,
        value: u8,
    },
    GetBit {
        key: String,
        offset: u64,
    },
    BitCount {
        key: String,
        range: Option<(i64, i64)>,
        bit_unit: bool,
    },
    BitPos {
        key: String,
        bit: u8,
        start: Option<i64>,
        end: Option<i64>,
        bit_unit: bool,
    },
    BitOp {
        op: String,
        dest: String,
        keys: Vec<String>,
    },
    PfAdd {
        key: String,
        elements: Vec<Vec<u8>>,
    },
    PfCount {
        keys: Vec<String>,
    },
    PfMerge {
        dest: String,
        sources: Vec<String>,
    },

    // Geo commands
    GeoAdd {
        key: String,
        nx: bool,
        xx: bool,
        ch: bool,
        items: Vec<(f64, f64, Vec<u8>)>, // (lon, lat, member)
    },
    GeoPos {
        key: String,
        members: Vec<Vec<u8>>,
    },
    GeoDist {
        key: String,
        member1: Vec<u8>,
        member2: Vec<u8>,
        unit: String,
    },
    GeoHash {
        key: String,
        members: Vec<Vec<u8>>,
    },
    GeoSearch {
        key: String,
        from_member: Option<Vec<u8>>,
        from_lonlat: Option<(f64, f64)>,
        radius: f64,
        unit: String,
        with_coord: bool,
        with_dist: bool,
        with_hash: bool,
        count: Option<usize>,
        asc: Option<bool>, // Some(true)=ASC, Some(false)=DESC, None=unordered
    },

    // Stream commands
    XAdd {
        key: String,
        id: String,
        nomkstream: bool,
        maxlen: Option<usize>,
        fields: Vec<(Vec<u8>, Vec<u8>)>,
    },
    XLen {
        key: String,
    },
    XRange {
        key: String,
        start: String,
        end: String,
        count: Option<usize>,
    },
    XRevRange {
        key: String,
        end: String,
        start: String,
        count: Option<usize>,
    },
    XDel {
        key: String,
        ids: Vec<String>,
    },
    XTrim {
        key: String,
        maxlen: usize,
    },
    XRead {
        count: Option<usize>,
        streams: Vec<(String, String)>, // (key, id) pairs
    },
    XGroupCreate {
        key: String,
        group: String,
        id: String,
        mkstream: bool,
    },
    XGroupSetId {
        key: String,
        group: String,
        id: String,
    },
    XGroupDestroy {
        key: String,
        group: String,
    },
    XGroupCreateConsumer {
        key: String,
        group: String,
        consumer: String,
    },
    XGroupDelConsumer {
        key: String,
        group: String,
        consumer: String,
    },
    XReadGroup {
        group: String,
        consumer: String,
        count: Option<usize>,
        noack: bool,
        streams: Vec<(String, String)>,
    },
    XAck {
        key: String,
        group: String,
        ids: Vec<String>,
    },
    XPending {
        key: String,
        group: String,
        start: Option<String>,
        end: Option<String>,
        count: Option<usize>,
        consumer: Option<String>,
        idle: Option<u64>,
    },
    XClaim {
        key: String,
        group: String,
        consumer: String,
        min_idle: u64,
        ids: Vec<String>,
        idle: Option<u64>,
        time: Option<u64>,
        retrycount: Option<u64>,
        force: bool,
        justid: bool,
    },
    XAutoClaim {
        key: String,
        group: String,
        consumer: String,
        min_idle: u64,
        start: String,
        count: Option<usize>,
        justid: bool,
    },
    XInfoStream {
        key: String,
    },
    XInfoGroups {
        key: String,
    },
    XInfoConsumers {
        key: String,
        group: String,
    },
    XSetId {
        key: String,
        id: String,
        entries_added: Option<u64>,
        max_deleted_id: Option<String>,
    },

    // Generic commands
    Type {
        key: String,
    },
    Keys {
        pattern: String,
    },
    RandomKey {},
    Rename {
        key: String,
        new_key: String,
    },
    RenameNx {
        key: String,
        new_key: String,
    },
    Copy {
        source: String,
        destination: String,
        destination_db: Option<u32>,
        replace: bool,
    },
    Move {
        key: String,
        db: u32,
    },
    Exists {
        keys: Vec<String>,
    },
    Touch {
        keys: Vec<String>,
    },
    Expire {
        key: String,
        seconds: u64,
    },
    PExpire {
        key: String,
        milliseconds: u64,
    },
    ExpireAt {
        key: String,
        timestamp: u64,
    },
    PExpireAt {
        key: String,
        milliseconds_timestamp: u64,
    },
    Ttl {
        key: String,
    },
    Pttl {
        key: String,
    },
    ExpireTime {
        key: String,
    },
    PExpireTime {
        key: String,
    },
    Persist {
        key: String,
    },
    ObjectEncoding {
        key: String,
    },
    ObjectRefCount {
        key: String,
    },
    ObjectIdleTime {
        key: String,
    },
    DbSize {},
    FlushDb {},
    FlushAll {},
    Select {
        db: u32,
    },
    Scan {
        cursor: u64,
        pattern: Option<String>,
        count: Option<usize>,
    },
    Wait {
        numreplicas: u64,
        timeout_ms: u64,
    },
    Del {
        keys: Vec<String>,
    },
    Unlink {
        keys: Vec<String>,
    },

    // Strings/Numbers commands
    // String values are binary-safe (raw bytes), matching Redis. (C1)
    Set {
        key: String,
        value: Vec<u8>,
        expire_option: Option<SetExpireOption>,
        set_option: Option<SetOption>,
    },
    SetNx {
        key: String,
        value: Vec<u8>,
    },
    SetEx {
        key: String,
        seconds: u64,
        value: Vec<u8>,
    },
    PSetEx {
        key: String,
        milliseconds: u64,
        value: Vec<u8>,
    },
    Get {
        key: String,
    },
    GetDel {
        key: String,
    },
    GetEx {
        key: String,
        option: Option<GetExOption>,
    },
    Append {
        key: String,
        value: Vec<u8>,
    },
    StrLen {
        key: String,
    },
    GetRange {
        key: String,
        start: isize,
        end: isize,
    },
    SetRange {
        key: String,
        offset: usize,
        value: Vec<u8>,
    },
    GetSet {
        key: String,
        value: Vec<u8>,
    },
    MGet {
        keys: Vec<String>,
    }, // MGET key [key ...]
    MSet {
        key_values: Vec<(String, Vec<u8>)>,
    }, // MSET key value [key value ...]
    MSetNx {
        key_values: Vec<(String, Vec<u8>)>,
    },
    SAdd {
        key: String,
        members: Vec<Vec<u8>>,
    },
    SRem {
        key: String,
        members: Vec<Vec<u8>>,
    },
    SMembers {
        key: String,
    },
    SIsMember {
        key: String,
        member: Vec<u8>,
    },
    SCard {
        key: String,
    },
    SPop {
        key: String,
        count: Option<usize>,
    },
    SRandMember {
        key: String,
        count: Option<isize>,
    },
    Incr {
        key: String,
    },
    Decr {
        key: String,
    },
    IncrBy {
        key: String,
        increment: i64,
    },
    DecrBy {
        key: String,
        decrement: i64,
    },
    IncrByFloat {
        key: String,
        increment: f64,
    },

    // Hash commands
    HSet {
        key: String,
        field_values: Vec<(Vec<u8>, Vec<u8>)>,
    }, // HSET key field value [field value ...]
    HGet {
        key: String,
        field: Vec<u8>,
    },
    HDel {
        key: String,
        fields: Vec<Vec<u8>>,
    }, // HDEL key field [field ...]
    HExists {
        key: String,
        field: Vec<u8>,
    },
    HGetAll {
        key: String,
    },
    HIncrBy {
        key: String,
        field: Vec<u8>,
        increment: i64,
    },
    HIncrByFloat {
        key: String,
        key_field_increment: Vec<(Vec<u8>, f64)>,
    }, //HINCRBYFLOAT key field increment [field increment ...]
    HKeys {
        key: String,
    },
    HLen {
        key: String,
    },
    HMGet {
        key: String,
        fields: Vec<Vec<u8>>,
    }, // HMGET key field [field ...]
    HMSet {
        key: String,
        field_values: Vec<(Vec<u8>, Vec<u8>)>,
    }, // HMSET key field value [field value ...]
    HSetNx {
        key: String,
        field: Vec<u8>,
        value: Vec<u8>,
    },
    HVals {
        key: String,
    },

    // List Commands
    ListLPush {
        key: String,
        values: Vec<Vec<u8>>,
    }, // LPUSH key value [value ...]
    ListRPush {
        key: String,
        values: Vec<Vec<u8>>,
    }, // RPUSH key value [value ...]
    LPop {
        key: String,
    },
    RPop {
        key: String,
    },
    LLen {
        key: String,
    },
    LRange {
        key: String,
        start: isize,
        end: isize,
    },
    LIndex {
        key: String,
        index: isize,
    },
    LInsert {
        key: String,
        before_after: InsertOption,
        pivot: Vec<u8>,
        value: Vec<u8>,
    }, // LINSERT key BEFORE|AFTER pivot value
    LSet {
        key: String,
        index: isize,
        value: Vec<u8>,
    },
    LTrim {
        key: String,
        start: isize,
        end: isize,
    },
    LRem {
        key: String,
        count: i64,
        value: Vec<u8>,
    },
    RPopLPush {
        source: String,
        destination: String,
    },
    BLPop {
        keys: Vec<String>,
        timeout: f64,
    }, // BLPOP key [key ...] timeout
    BRPop {
        keys: Vec<String>,
        timeout: f64,
    }, // BRPOP key [key ...] timeout

    // JSON Commands
    JsonSet {
        key: String,
        path: String,
        value: String,
    }, // JSON.SET key path value
    JsonGet {
        key: String,
        path: String,
    }, // JSON.GET key [path]
    JsonDel {
        key: String,
        path: String,
    }, // JSON.DEL key [path]
    JsonType {
        key: String,
        path: String,
    }, // JSON.TYPE key [path]
    JsonNumIncrBy {
        key: String,
        path: String,
        increment: f64,
    }, // JSON.NUMINCRBY key path increment
    JsonStrAppend {
        key: String,
        path: String,
        value: String,
    }, // JSON.STRAPPEND key path value
    JsonArrAppend {
        key: String,
        path: String,
        values: Vec<String>,
    }, // JSON.ARRAPPEND key path value [value ...]
    JsonObjSet {
        key: String,
        path: String,
        key_to_set: String,
        value: String,
    }, // JSON.OBJSET key path key value
    JsonObjKeys {
        key: String,
        path: String,
    }, // JSON.OBJKEYS key [path]
    JsonObjLen {
        key: String,
        path: String,
    }, // JSON.OBJLEN key [path]
    JsonArrIndex {
        key: String,
        path: String,
        value: String,
        range: Option<(isize, isize)>,
    }, // JSON.ARRINDEX key path value [start [stop]]
    JsonArrInsert {
        key: String,
        path: String,
        index: isize,
        values: Vec<String>,
    }, // JSON.ARRINSERT key path index value [value ...]
    JsonArrLen {
        key: String,
        path: String,
    }, // JSON.ARRLEN key [path]
    JsonArrPop {
        key: String,
        path: String,
        index: Option<isize>,
    }, // JSON.ARRPOP key path [index]
    JsonArrTrim {
        key: String,
        path: String,
        start: isize,
        stop: isize,
    }, // JSON.ARRTRIM key path start stop

    // Graph Commands
    GraphCreateNode {
        key: String,
        node_id: String,
        properties: Vec<(String, String)>,
    }, // GRAPH.CREATE_NODE key node_id [property key value ...]
    GraphGetNode {
        key: String,
        node_id: String,
    }, // GRAPH.GET_NODE key node_id
    GraphDeleteNode {
        key: String,
        node_id: String,
    }, // GRAPH.DELETE_NODE key node_id

    GraphCreateEdge {
        key: String,
        edge_id: String,
        source_node_id: String,
        target_node_id: String,
        relation_type: String,
        properties: Vec<(String, String)>,
    },
    GraphGetEdge {
        key: String,
        edge_id: String,
    },
    GraphDeleteEdge {
        key: String,
        edge_id: String,
    },

    GraphGetNodeProperties {
        key: String,
        node_id: String,
    }, // GRAPH.GET_NODE_PROPERTIES key node_id
    GraphSetNodeProperty {
        key: String,
        node_id: String,
        property_key: String,
        property_value: String,
    }, // GRAPH.SET_NODE_PROPERTY key node_id property_key property_value
    GraphDeleteNodeProperty {
        key: String,
        node_id: String,
        property_key: String,
    }, // GRAPH.DELETE_NODE_PROPERTY key node_id property_key

    GraphGetEdgeProperties {
        key: String,
        edge_id: String,
    }, // GRAPH.GET_EDGE_PROPERTIES key edge_id
    GraphSetEdgeProperty {
        key: String,
        edge_id: String,
        property_key: String,
        property_value: String,
    }, // GRAPH.SET_EDGE_PROPERTY key edge_id property_key
    GraphDeleteEdgeProperty {
        key: String,
        edge_id: String,
        property_key: String,
    }, // GRAPH.DELETE_EDGE_PROPERTY key edge_id property_key

    Unknown(String),
}

impl TryFrom<&[Value]> for Command {
    type Error = &'static str;

    fn try_from(values: &[Value]) -> Result<Self, Self::Error> {
        if values.is_empty() {
            return Err("Empty Command");
        }

        match &values[0] {
            Value::BulkString(bs) => {
                match std::str::from_utf8(bs) {
                    Ok(command_str) => {
                        let command_upper = command_str.to_uppercase();
                        match command_upper.as_str() {
                            // connection commands
                            "PING" => {
                                if values.len() == 1 {
                                    Ok(Command::Ping {})
                                } else if values.len() == 2 {
                                    if let Value::BulkString(msg_bs) = &values[1] {
                                        let message = String::from_utf8_lossy(msg_bs).into_owned();
                                        Ok(Command::Echo { message })
                                    } else {
                                        Err("PING message argument must be a bulk string")
                                    }
                                } else {
                                    Err("PING command expects no arguments or one message argument")
                                }
                            }
                            "QUIT" => {
                                if values.len() == 1 {
                                    Ok(Command::Quit {})
                                } else {
                                    Err("QUIT command expects no arguments")
                                }
                            }
                            "HELLO" => {
                                // HELLO [protover] [AUTH <user> <pass>] [SETNAME <name>]
                                let mut protover: Option<u8> = None;
                                let mut auth: Option<(String, String)> = None;
                                let mut setname: Option<String> = None;
                                let mut idx = 1;

                                // Optional protocol version is the first positional arg.
                                if values.len() > idx {
                                    if let Value::BulkString(bs) = &values[idx] {
                                        let s = String::from_utf8_lossy(bs);
                                        if let Ok(v) = s.parse::<u8>() {
                                            protover = Some(v);
                                            idx += 1;
                                        } else if s
                                            .chars()
                                            .next()
                                            .is_some_and(|c| c.is_ascii_digit())
                                        {
                                            return Err("HELLO protover must be an integer");
                                        }
                                    } else {
                                        return Err("HELLO arguments must be bulk strings");
                                    }
                                }

                                // Order-independent option keywords.
                                while idx < values.len() {
                                    let kw = match &values[idx] {
                                        Value::BulkString(bs) => {
                                            String::from_utf8_lossy(bs).to_uppercase()
                                        }
                                        _ => return Err("HELLO arguments must be bulk strings"),
                                    };
                                    match kw.as_str() {
                                        "AUTH" => {
                                            if idx + 2 >= values.len() {
                                                return Err(
                                                    "HELLO AUTH requires username and password",
                                                );
                                            }
                                            let user =
                                                match &values[idx + 1] {
                                                    Value::BulkString(bs) => {
                                                        String::from_utf8_lossy(bs).into_owned()
                                                    }
                                                    _ => return Err(
                                                        "HELLO AUTH username must be a bulk string",
                                                    ),
                                                };
                                            let pass =
                                                match &values[idx + 2] {
                                                    Value::BulkString(bs) => {
                                                        String::from_utf8_lossy(bs).into_owned()
                                                    }
                                                    _ => return Err(
                                                        "HELLO AUTH password must be a bulk string",
                                                    ),
                                                };
                                            auth = Some((user, pass));
                                            idx += 3;
                                        }
                                        "SETNAME" => {
                                            if idx + 1 >= values.len() {
                                                return Err("HELLO SETNAME requires a name");
                                            }
                                            let name =
                                                match &values[idx + 1] {
                                                    Value::BulkString(bs) => {
                                                        String::from_utf8_lossy(bs).into_owned()
                                                    }
                                                    _ => return Err(
                                                        "HELLO SETNAME name must be a bulk string",
                                                    ),
                                                };
                                            setname = Some(name);
                                            idx += 2;
                                        }
                                        _ => return Err("HELLO Syntax error in HELLO"),
                                    }
                                }

                                Ok(Command::Hello {
                                    protover,
                                    auth,
                                    setname,
                                })
                            }
                            "ECHO" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(msg_bs) = &values[1] {
                                        let message = String::from_utf8_lossy(msg_bs).into_owned();
                                        Ok(Command::Echo { message })
                                    } else {
                                        Err("ECHO command argument must be a bulk string")
                                    }
                                } else {
                                    Err("ECHO command expects exactly one argument")
                                }
                            }
                            "INFO" => {
                                if values.len() == 1 {
                                    Ok(Command::Info { section: None })
                                } else if values.len() == 2 {
                                    if let Value::BulkString(section_bs) = &values[1] {
                                        let section =
                                            String::from_utf8_lossy(section_bs).into_owned();
                                        Ok(Command::Info {
                                            section: Some(section),
                                        })
                                    } else {
                                        Err("INFO section must be a bulk string")
                                    }
                                } else {
                                    Err("INFO command expects zero or one section argument")
                                }
                            }
                            "AUTH" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(password_bs) = &values[1] {
                                        let password =
                                            String::from_utf8_lossy(password_bs).into_owned();
                                        Ok(Command::Auth { password })
                                    } else {
                                        Err("AUTH password must be a bulk string")
                                    }
                                } else {
                                    Err("AUTH command expects exactly one argument (password)")
                                }
                            }
                            "COMMAND" => {
                                // COUNT and DOCS accept any arity, so resolve them
                                // before the length-based dispatch. Clients
                                // (notably redis-py) call `COMMAND DOCS` on connect.
                                if values.len() >= 2 {
                                    if let Value::BulkString(sub_bs) = &values[1] {
                                        match String::from_utf8_lossy(sub_bs)
                                            .to_uppercase()
                                            .as_str()
                                        {
                                            "COUNT" => return Ok(Command::CommandCount {}),
                                            "DOCS" => {
                                                let command_names = values[2..]
                                                    .iter()
                                                    .filter_map(|v| match v {
                                                        Value::BulkString(b) => Some(
                                                            String::from_utf8_lossy(b).into_owned(),
                                                        ),
                                                        _ => None,
                                                    })
                                                    .collect();
                                                return Ok(Command::CommandDocs { command_names });
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                if values.len() == 1 {
                                    Ok(Command::Command {})
                                } else if values.len() == 2 {
                                    if let Value::BulkString(sub_command_bs) = &values[1] {
                                        let sub_command =
                                            String::from_utf8_lossy(sub_command_bs).to_uppercase();
                                        match sub_command.as_str() {
                                            "LIST" => Ok(Command::CommandList {}),
                                            "HELP" => {
                                                Err("COMMAND HELP expects a command name argument")
                                            } // COMMAND HELP without command name is invalid
                                            "INFO" => {
                                                Err("COMMAND INFO expects a command name argument")
                                            } // COMMAND INFO without command name is invalid
                                            _ => Ok(Command::Unknown(command_str.to_string())), // Treat as unknown if subcommand not recognized
                                        }
                                    } else {
                                        Err("COMMAND subcommand must be a bulk string")
                                    }
                                } else if values.len() == 3 {
                                    if let Value::BulkString(sub_command_bs) = &values[1] {
                                        let sub_command =
                                            String::from_utf8_lossy(sub_command_bs).to_uppercase();
                                        if let Value::BulkString(command_name_bs) = &values[2] {
                                            let command_name =
                                                String::from_utf8_lossy(command_name_bs)
                                                    .into_owned();
                                            match sub_command.as_str() {
                                                "INFO" => Ok(Command::CommandInfo { command_name }),
                                                "HELP" => Ok(Command::CommandHelp { command_name }),
                                                _ => Ok(Command::Unknown(command_str.to_string())), // Treat as unknown if subcommand not recognized
                                            }
                                        } else {
                                            Err("COMMAND INFO/HELP command name argument must be a bulk string")
                                        }
                                    } else {
                                        Err("COMMAND subcommand must be a bulk string")
                                    }
                                } else {
                                    Ok(Command::Command {}) // Default COMMAND with no or invalid args
                                }
                            }

                            "CLIENT" => {
                                if values.len() == 1 {
                                    Ok(Command::Client {
                                        subcommand: ClientSubCommand::NoSubCommand,
                                    }) // Bare CLIENT command
                                } else if values.len() >= 2 {
                                    if let Value::BulkString(sub_command_bs) = &values[1] {
                                        let sub_command_str =
                                            String::from_utf8_lossy(sub_command_bs).to_uppercase();
                                        match sub_command_str.as_str() {
                                            "LIST" => Ok(Command::Client {
                                                subcommand: ClientSubCommand::List,
                                            }),
                                            "INFO" => Ok(Command::Client {
                                                subcommand: ClientSubCommand::Info,
                                            }),
                                            "GETNAME" => Ok(Command::Client {
                                                subcommand: ClientSubCommand::GetName,
                                            }),
                                            "SETNAME" => {
                                                if values.len() == 3 {
                                                    if let Value::BulkString(connection_name_bs) =
                                                        &values[2]
                                                    {
                                                        let connection_name =
                                                            String::from_utf8_lossy(
                                                                connection_name_bs,
                                                            )
                                                            .into_owned();
                                                        Ok(Command::Client {
                                                            subcommand: ClientSubCommand::SetName {
                                                                connection_name,
                                                            },
                                                        })
                                                    } else {
                                                        Err("CLIENT SETNAME connection name must be a bulk string")
                                                    }
                                                } else {
                                                    Err("CLIENT SETNAME expects exactly one argument (connection name)")
                                                }
                                            }
                                            "KILL" => {
                                                if values.len() == 3 {
                                                    if let Value::BulkString(ip_port_bs) =
                                                        &values[2]
                                                    {
                                                        let ip_port =
                                                            String::from_utf8_lossy(ip_port_bs)
                                                                .into_owned();
                                                        Ok(Command::Client {
                                                            subcommand: ClientSubCommand::Kill {
                                                                ip_port,
                                                            },
                                                        })
                                                    } else {
                                                        Err("CLIENT KILL ip:port must be a bulk string")
                                                    }
                                                } else {
                                                    Err("CLIENT KILL expects exactly one argument (ip:port)")
                                                }
                                            }

                                            "ID" => Ok(Command::Client {
                                                subcommand: ClientSubCommand::Id,
                                            }),
                                            "SETINFO" => Ok(Command::Client {
                                                subcommand: ClientSubCommand::SetInfo,
                                            }),
                                            "NO-EVICT" => Ok(Command::Client {
                                                subcommand: ClientSubCommand::NoEvict,
                                            }),
                                            "NO-TOUCH" => Ok(Command::Client {
                                                subcommand: ClientSubCommand::NoTouch,
                                            }),
                                            "UNPAUSE" => Ok(Command::Client {
                                                subcommand: ClientSubCommand::Unpause,
                                            }),
                                            _ => Ok(Command::Client {
                                                subcommand: ClientSubCommand::NoSubCommand,
                                            }), // Treat as CLIENT with no subcommand if not recognized
                                        }
                                    } else {
                                        Err("CLIENT subcommand must be a bulk string")
                                    }
                                } else {
                                    Ok(Command::Client {
                                        subcommand: ClientSubCommand::NoSubCommand,
                                    }) // Default CLIENT
                                }
                            }

                            "CONFIG" => {
                                if values.len() < 2 {
                                    return Err("CONFIG subcommand required");
                                }
                                let sub = match &values[1] {
                                    Value::BulkString(b) => {
                                        String::from_utf8_lossy(b).to_uppercase()
                                    }
                                    _ => return Err("CONFIG subcommand must be a bulk string"),
                                };
                                match sub.as_str() {
                                    "GET" => {
                                        if values.len() < 3 {
                                            return Err(
                                                "CONFIG GET requires at least one parameter",
                                            );
                                        }
                                        let params = values[2..]
                                            .iter()
                                            .filter_map(|v| match v {
                                                Value::BulkString(b) => {
                                                    Some(String::from_utf8_lossy(b).into_owned())
                                                }
                                                _ => None,
                                            })
                                            .collect();
                                        Ok(Command::Config {
                                            sub: ConfigSubCommand::Get { params },
                                        })
                                    }
                                    "SET" => {
                                        if values.len() < 4 || !(values.len() - 2).is_multiple_of(2)
                                        {
                                            return Err(
                                                "CONFIG SET requires one or more param value pairs",
                                            );
                                        }
                                        let mut pairs = Vec::new();
                                        let mut i = 2;
                                        while i + 1 < values.len() {
                                            let k = match &values[i] {
                                                Value::BulkString(b) => {
                                                    String::from_utf8_lossy(b).into_owned()
                                                }
                                                _ => return Err(
                                                    "CONFIG SET parameter must be a bulk string",
                                                ),
                                            };
                                            let v =
                                                match &values[i + 1] {
                                                    Value::BulkString(b) => {
                                                        String::from_utf8_lossy(b).into_owned()
                                                    }
                                                    _ => return Err(
                                                        "CONFIG SET value must be a bulk string",
                                                    ),
                                                };
                                            pairs.push((k, v));
                                            i += 2;
                                        }
                                        Ok(Command::Config {
                                            sub: ConfigSubCommand::Set { pairs },
                                        })
                                    }
                                    "RESETSTAT" => Ok(Command::Config {
                                        sub: ConfigSubCommand::ResetStat,
                                    }),
                                    "REWRITE" => Ok(Command::Config {
                                        sub: ConfigSubCommand::Rewrite,
                                    }),
                                    _ => Err(
                                        "Unknown CONFIG subcommand or wrong number of arguments",
                                    ),
                                }
                            }

                            // Pub/Sub Commands
                            "SUBSCRIBE" => {
                                if values.len() >= 2 {
                                    let mut channels = Vec::with_capacity(values.len() - 1);
                                    for v in &values[1..] {
                                        if let Value::BulkString(b) = v {
                                            channels.push(String::from_utf8_lossy(b).into_owned());
                                        } else {
                                            return Err("SUBSCRIBE channels must be bulk strings");
                                        }
                                    }
                                    Ok(Command::Subscribe { channels })
                                } else {
                                    Err("SUBSCRIBE command expects at least one channel")
                                }
                            }
                            "UNSUBSCRIBE" => {
                                let mut channels = Vec::new();
                                for v in &values[1..] {
                                    if let Value::BulkString(b) = v {
                                        channels.push(String::from_utf8_lossy(b).into_owned());
                                    } else {
                                        return Err("UNSUBSCRIBE channels must be bulk strings");
                                    }
                                }
                                Ok(Command::Unsubscribe { channels })
                            }
                            "PSUBSCRIBE" => {
                                if values.len() >= 2 {
                                    let mut patterns = Vec::with_capacity(values.len() - 1);
                                    for v in &values[1..] {
                                        if let Value::BulkString(b) = v {
                                            patterns.push(String::from_utf8_lossy(b).into_owned());
                                        } else {
                                            return Err("PSUBSCRIBE patterns must be bulk strings");
                                        }
                                    }
                                    Ok(Command::PSubscribe { patterns })
                                } else {
                                    Err("PSUBSCRIBE command expects at least one pattern")
                                }
                            }
                            "PUNSUBSCRIBE" => {
                                let mut patterns = Vec::new();
                                for v in &values[1..] {
                                    if let Value::BulkString(b) = v {
                                        patterns.push(String::from_utf8_lossy(b).into_owned());
                                    } else {
                                        return Err("PUNSUBSCRIBE patterns must be bulk strings");
                                    }
                                }
                                Ok(Command::PUnsubscribe { patterns })
                            }
                            "PUBLISH" => {
                                if values.len() == 3 {
                                    let channel = match &values[1] {
                                        Value::BulkString(b) => {
                                            String::from_utf8_lossy(b).into_owned()
                                        }
                                        _ => return Err("PUBLISH channel must be a bulk string"),
                                    };
                                    let message = match &values[2] {
                                        Value::BulkString(b) => b.clone(),
                                        _ => return Err("PUBLISH message must be a bulk string"),
                                    };
                                    Ok(Command::Publish { channel, message })
                                } else {
                                    Err("PUBLISH command expects exactly two arguments (channel message)")
                                }
                            }
                            "PUBSUB" => {
                                if values.len() < 2 {
                                    return Err("PUBSUB subcommand required");
                                }
                                let sub = match &values[1] {
                                    Value::BulkString(b) => {
                                        String::from_utf8_lossy(b).to_uppercase()
                                    }
                                    _ => return Err("PUBSUB subcommand must be a bulk string"),
                                };
                                match sub.as_str() {
                                    "CHANNELS" => {
                                        let pattern = if values.len() > 2 {
                                            match &values[2] {
                                                Value::BulkString(b) => {
                                                    Some(String::from_utf8_lossy(b).into_owned())
                                                }
                                                _ => return Err(
                                                    "PUBSUB CHANNELS pattern must be a bulk string",
                                                ),
                                            }
                                        } else {
                                            None
                                        };
                                        Ok(Command::PubSubChannels { pattern })
                                    }
                                    "NUMSUB" => {
                                        let mut channels = Vec::new();
                                        for v in &values[2..] {
                                            if let Value::BulkString(b) = v {
                                                channels
                                                    .push(String::from_utf8_lossy(b).into_owned());
                                            } else {
                                                return Err(
                                                    "PUBSUB NUMSUB channels must be bulk strings",
                                                );
                                            }
                                        }
                                        Ok(Command::PubSubNumSub { channels })
                                    }
                                    "NUMPAT" => Ok(Command::PubSubNumPat {}),
                                    _ => Err("Unknown PUBSUB subcommand"),
                                }
                            }

                            // Transaction Commands
                            "MULTI" => {
                                if values.len() == 1 {
                                    Ok(Command::Multi {})
                                } else {
                                    Err("MULTI command expects no arguments")
                                }
                            }
                            "EXEC" => {
                                if values.len() == 1 {
                                    Ok(Command::Exec {})
                                } else {
                                    Err("EXEC command expects no arguments")
                                }
                            }
                            "DISCARD" => {
                                if values.len() == 1 {
                                    Ok(Command::Discard {})
                                } else {
                                    Err("DISCARD command expects no arguments")
                                }
                            }
                            "UNWATCH" => {
                                if values.len() == 1 {
                                    Ok(Command::Unwatch {})
                                } else {
                                    Err("UNWATCH command expects no arguments")
                                }
                            }
                            "WATCH" => {
                                if values.len() >= 2 {
                                    let mut keys = Vec::with_capacity(values.len() - 1);
                                    for v in &values[1..] {
                                        if let Value::BulkString(b) = v {
                                            keys.push(String::from_utf8_lossy(b).into_owned());
                                        } else {
                                            return Err("WATCH keys must be bulk strings");
                                        }
                                    }
                                    Ok(Command::Watch { keys })
                                } else {
                                    Err("WATCH command expects at least one key")
                                }
                            }

                            // Sorted set commands
                            "ZADD" => {
                                if values.len() < 4 {
                                    return Err("ZADD command expects key [NX|XX] [GT|LT] [CH] [INCR] score member [score member ...]");
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("ZADD key must be a bulk string");
                                };

                                let mut nx = false;
                                let mut xx = false;
                                let mut gt = false;
                                let mut lt = false;
                                let mut ch = false;
                                let mut incr = false;

                                // Scan optional flags after the key until a token parses as a float.
                                let mut i = 2;
                                while i < values.len() {
                                    let bs = if let Value::BulkString(bs) = &values[i] {
                                        bs
                                    } else {
                                        return Err("ZADD arguments must be bulk strings");
                                    };
                                    // A token that parses as a float ends the flag section
                                    // and marks the start of score/member pairs.
                                    if String::from_utf8_lossy(bs).parse::<f64>().is_ok() {
                                        break;
                                    }
                                    match String::from_utf8_lossy(bs).to_uppercase().as_str() {
                                        "NX" => nx = true,
                                        "XX" => xx = true,
                                        "GT" => gt = true,
                                        "LT" => lt = true,
                                        "CH" => ch = true,
                                        "INCR" => incr = true,
                                        _ => return Err("ZADD score is not a valid float"),
                                    }
                                    i += 1;
                                }

                                // Remaining tokens are score/member pairs.
                                if i >= values.len() {
                                    return Err(
                                        "ZADD command expects at least one score member pair",
                                    );
                                }
                                if !(values.len() - i).is_multiple_of(2) {
                                    return Err(
                                        "ZADD command expects score member pairs (even count)",
                                    );
                                }

                                let mut members: Vec<(f64, Vec<u8>)> = Vec::new();
                                while i < values.len() {
                                    let score = if let Value::BulkString(score_bs) = &values[i] {
                                        String::from_utf8_lossy(score_bs)
                                            .parse::<f64>()
                                            .map_err(|_| "ZADD score is not a valid float")?
                                    } else {
                                        return Err("ZADD score must be a bulk string");
                                    };
                                    let member =
                                        if let Value::BulkString(member_bs) = &values[i + 1] {
                                            member_bs.clone()
                                        } else {
                                            return Err("ZADD member must be a bulk string");
                                        };
                                    members.push((score, member));
                                    i += 2;
                                }

                                Ok(Command::ZAdd {
                                    key,
                                    nx,
                                    xx,
                                    gt,
                                    lt,
                                    ch,
                                    incr,
                                    members,
                                })
                            }
                            "ZSCORE" => {
                                if values.len() == 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZSCORE key must be a bulk string");
                                    };
                                    let member = if let Value::BulkString(member_bs) = &values[2] {
                                        member_bs.clone()
                                    } else {
                                        return Err("ZSCORE member must be a bulk string");
                                    };
                                    Ok(Command::ZScore { key, member })
                                } else {
                                    Err("ZSCORE command expects exactly two arguments (key member)")
                                }
                            }
                            "ZMSCORE" => {
                                if values.len() >= 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZMSCORE key must be a bulk string");
                                    };
                                    let mut members = Vec::with_capacity(values.len() - 2);
                                    for v in &values[2..] {
                                        if let Value::BulkString(m_bs) = v {
                                            members.push(m_bs.clone());
                                        } else {
                                            return Err("ZMSCORE members must be bulk strings");
                                        }
                                    }
                                    Ok(Command::ZMScore { key, members })
                                } else {
                                    Err("ZMSCORE command expects at least two arguments (key member [member ...])")
                                }
                            }
                            "ZCARD" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::ZCard { key })
                                    } else {
                                        Err("ZCARD key must be a bulk string")
                                    }
                                } else {
                                    Err("ZCARD command expects exactly one argument (key)")
                                }
                            }
                            "ZINCRBY" => {
                                if values.len() == 4 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZINCRBY key must be a bulk string");
                                    };
                                    let increment = if let Value::BulkString(inc_bs) = &values[2] {
                                        String::from_utf8_lossy(inc_bs)
                                            .parse::<f64>()
                                            .map_err(|_| "ZINCRBY increment is not a valid float")?
                                    } else {
                                        return Err("ZINCRBY increment must be a bulk string");
                                    };
                                    let member = if let Value::BulkString(member_bs) = &values[3] {
                                        member_bs.clone()
                                    } else {
                                        return Err("ZINCRBY member must be a bulk string");
                                    };
                                    Ok(Command::ZIncrBy {
                                        key,
                                        increment,
                                        member,
                                    })
                                } else {
                                    Err("ZINCRBY command expects exactly three arguments (key increment member)")
                                }
                            }
                            "ZREM" => {
                                if values.len() >= 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZREM key must be a bulk string");
                                    };
                                    let mut members = Vec::with_capacity(values.len() - 2);
                                    for v in &values[2..] {
                                        if let Value::BulkString(m_bs) = v {
                                            members.push(m_bs.clone());
                                        } else {
                                            return Err("ZREM members must be bulk strings");
                                        }
                                    }
                                    Ok(Command::ZRem { key, members })
                                } else {
                                    Err("ZREM command expects at least two arguments (key member [member ...])")
                                }
                            }
                            "ZRANK" => {
                                if values.len() == 3 || values.len() == 4 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZRANK key must be a bulk string");
                                    };
                                    let member = if let Value::BulkString(member_bs) = &values[2] {
                                        member_bs.clone()
                                    } else {
                                        return Err("ZRANK member must be a bulk string");
                                    };
                                    let with_score = if values.len() == 4 {
                                        if let Value::BulkString(opt_bs) = &values[3] {
                                            if String::from_utf8_lossy(opt_bs).to_uppercase()
                                                == "WITHSCORE"
                                            {
                                                true
                                            } else {
                                                return Err(
                                                    "ZRANK only supports the WITHSCORE option",
                                                );
                                            }
                                        } else {
                                            return Err("ZRANK option must be a bulk string");
                                        }
                                    } else {
                                        false
                                    };
                                    Ok(Command::ZRank {
                                        key,
                                        member,
                                        with_score,
                                    })
                                } else {
                                    Err("ZRANK command expects key member [WITHSCORE]")
                                }
                            }
                            "ZREVRANK" => {
                                if values.len() == 3 || values.len() == 4 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZREVRANK key must be a bulk string");
                                    };
                                    let member = if let Value::BulkString(member_bs) = &values[2] {
                                        member_bs.clone()
                                    } else {
                                        return Err("ZREVRANK member must be a bulk string");
                                    };
                                    let with_score = if values.len() == 4 {
                                        if let Value::BulkString(opt_bs) = &values[3] {
                                            if String::from_utf8_lossy(opt_bs).to_uppercase()
                                                == "WITHSCORE"
                                            {
                                                true
                                            } else {
                                                return Err(
                                                    "ZREVRANK only supports the WITHSCORE option",
                                                );
                                            }
                                        } else {
                                            return Err("ZREVRANK option must be a bulk string");
                                        }
                                    } else {
                                        false
                                    };
                                    Ok(Command::ZRevRank {
                                        key,
                                        member,
                                        with_score,
                                    })
                                } else {
                                    Err("ZREVRANK command expects key member [WITHSCORE]")
                                }
                            }
                            "ZRANGE" => {
                                if values.len() == 4 || values.len() == 5 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZRANGE key must be a bulk string");
                                    };
                                    let start = if let Value::BulkString(start_bs) = &values[2] {
                                        String::from_utf8_lossy(start_bs)
                                            .parse::<i64>()
                                            .map_err(|_| "ZRANGE start is not an integer")?
                                    } else {
                                        return Err("ZRANGE start must be a bulk string");
                                    };
                                    let stop = if let Value::BulkString(stop_bs) = &values[3] {
                                        String::from_utf8_lossy(stop_bs)
                                            .parse::<i64>()
                                            .map_err(|_| "ZRANGE stop is not an integer")?
                                    } else {
                                        return Err("ZRANGE stop must be a bulk string");
                                    };
                                    let with_scores = if values.len() == 5 {
                                        if let Value::BulkString(opt_bs) = &values[4] {
                                            if String::from_utf8_lossy(opt_bs).to_uppercase()
                                                == "WITHSCORES"
                                            {
                                                true
                                            } else {
                                                return Err(
                                                    "ZRANGE only supports the WITHSCORES option",
                                                );
                                            }
                                        } else {
                                            return Err("ZRANGE option must be a bulk string");
                                        }
                                    } else {
                                        false
                                    };
                                    Ok(Command::ZRange {
                                        key,
                                        start,
                                        stop,
                                        with_scores,
                                    })
                                } else {
                                    Err("ZRANGE command expects key start stop [WITHSCORES]")
                                }
                            }
                            "ZREVRANGE" => {
                                if values.len() == 4 || values.len() == 5 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZREVRANGE key must be a bulk string");
                                    };
                                    let start = if let Value::BulkString(start_bs) = &values[2] {
                                        String::from_utf8_lossy(start_bs)
                                            .parse::<i64>()
                                            .map_err(|_| "ZREVRANGE start is not an integer")?
                                    } else {
                                        return Err("ZREVRANGE start must be a bulk string");
                                    };
                                    let stop = if let Value::BulkString(stop_bs) = &values[3] {
                                        String::from_utf8_lossy(stop_bs)
                                            .parse::<i64>()
                                            .map_err(|_| "ZREVRANGE stop is not an integer")?
                                    } else {
                                        return Err("ZREVRANGE stop must be a bulk string");
                                    };
                                    let with_scores = if values.len() == 5 {
                                        if let Value::BulkString(opt_bs) = &values[4] {
                                            if String::from_utf8_lossy(opt_bs).to_uppercase()
                                                == "WITHSCORES"
                                            {
                                                true
                                            } else {
                                                return Err(
                                                    "ZREVRANGE only supports the WITHSCORES option",
                                                );
                                            }
                                        } else {
                                            return Err("ZREVRANGE option must be a bulk string");
                                        }
                                    } else {
                                        false
                                    };
                                    Ok(Command::ZRevRange {
                                        key,
                                        start,
                                        stop,
                                        with_scores,
                                    })
                                } else {
                                    Err("ZREVRANGE command expects key start stop [WITHSCORES]")
                                }
                            }
                            "ZRANGEBYSCORE" => {
                                if values.len() < 4 {
                                    return Err("ZRANGEBYSCORE command expects key min max [WITHSCORES] [LIMIT offset count]");
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("ZRANGEBYSCORE key must be a bulk string");
                                };
                                let min = if let Value::BulkString(min_bs) = &values[2] {
                                    String::from_utf8_lossy(min_bs).into_owned()
                                } else {
                                    return Err("ZRANGEBYSCORE min must be a bulk string");
                                };
                                let max = if let Value::BulkString(max_bs) = &values[3] {
                                    String::from_utf8_lossy(max_bs).into_owned()
                                } else {
                                    return Err("ZRANGEBYSCORE max must be a bulk string");
                                };
                                let mut with_scores = false;
                                let mut limit: Option<(i64, i64)> = None;
                                let mut i = 4;
                                while i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err(
                                            "ZRANGEBYSCORE option token must be a bulk string",
                                        );
                                    };
                                    match token.as_str() {
                                        "WITHSCORES" => {
                                            with_scores = true;
                                            i += 1;
                                        }
                                        "LIMIT" => {
                                            if i + 2 >= values.len() {
                                                return Err(
                                                    "ZRANGEBYSCORE LIMIT requires offset and count",
                                                );
                                            }
                                            let offset = if let Value::BulkString(off_bs) =
                                                &values[i + 1]
                                            {
                                                String::from_utf8_lossy(off_bs)
                                                    .parse::<i64>()
                                                    .map_err(|_| "ZRANGEBYSCORE LIMIT offset is not an integer")?
                                            } else {
                                                return Err("ZRANGEBYSCORE LIMIT offset must be a bulk string");
                                            };
                                            let count = if let Value::BulkString(cnt_bs) =
                                                &values[i + 2]
                                            {
                                                String::from_utf8_lossy(cnt_bs)
                                                    .parse::<i64>()
                                                    .map_err(|_| "ZRANGEBYSCORE LIMIT count is not an integer")?
                                            } else {
                                                return Err("ZRANGEBYSCORE LIMIT count must be a bulk string");
                                            };
                                            limit = Some((offset, count));
                                            i += 3;
                                        }
                                        _ => {
                                            return Err("ZRANGEBYSCORE only supports WITHSCORES and LIMIT options");
                                        }
                                    }
                                }
                                Ok(Command::ZRangeByScore {
                                    key,
                                    min,
                                    max,
                                    with_scores,
                                    limit,
                                })
                            }
                            "ZREVRANGEBYSCORE" => {
                                if values.len() < 4 {
                                    return Err("ZREVRANGEBYSCORE command expects key max min [WITHSCORES] [LIMIT offset count]");
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("ZREVRANGEBYSCORE key must be a bulk string");
                                };
                                let max = if let Value::BulkString(max_bs) = &values[2] {
                                    String::from_utf8_lossy(max_bs).into_owned()
                                } else {
                                    return Err("ZREVRANGEBYSCORE max must be a bulk string");
                                };
                                let min = if let Value::BulkString(min_bs) = &values[3] {
                                    String::from_utf8_lossy(min_bs).into_owned()
                                } else {
                                    return Err("ZREVRANGEBYSCORE min must be a bulk string");
                                };
                                let mut with_scores = false;
                                let mut limit: Option<(i64, i64)> = None;
                                let mut i = 4;
                                while i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err(
                                            "ZREVRANGEBYSCORE option token must be a bulk string",
                                        );
                                    };
                                    match token.as_str() {
                                        "WITHSCORES" => {
                                            with_scores = true;
                                            i += 1;
                                        }
                                        "LIMIT" => {
                                            if i + 2 >= values.len() {
                                                return Err("ZREVRANGEBYSCORE LIMIT requires offset and count");
                                            }
                                            let offset = if let Value::BulkString(off_bs) =
                                                &values[i + 1]
                                            {
                                                String::from_utf8_lossy(off_bs)
                                                    .parse::<i64>()
                                                    .map_err(|_| "ZREVRANGEBYSCORE LIMIT offset is not an integer")?
                                            } else {
                                                return Err("ZREVRANGEBYSCORE LIMIT offset must be a bulk string");
                                            };
                                            let count = if let Value::BulkString(cnt_bs) =
                                                &values[i + 2]
                                            {
                                                String::from_utf8_lossy(cnt_bs)
                                                    .parse::<i64>()
                                                    .map_err(|_| "ZREVRANGEBYSCORE LIMIT count is not an integer")?
                                            } else {
                                                return Err("ZREVRANGEBYSCORE LIMIT count must be a bulk string");
                                            };
                                            limit = Some((offset, count));
                                            i += 3;
                                        }
                                        _ => {
                                            return Err("ZREVRANGEBYSCORE only supports WITHSCORES and LIMIT options");
                                        }
                                    }
                                }
                                Ok(Command::ZRevRangeByScore {
                                    key,
                                    max,
                                    min,
                                    with_scores,
                                    limit,
                                })
                            }
                            "ZRANGEBYLEX" => {
                                if values.len() != 4 && values.len() != 7 {
                                    return Err("ZRANGEBYLEX command expects key min max [LIMIT offset count]");
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("ZRANGEBYLEX key must be a bulk string");
                                };
                                let min = if let Value::BulkString(min_bs) = &values[2] {
                                    String::from_utf8_lossy(min_bs).into_owned()
                                } else {
                                    return Err("ZRANGEBYLEX min must be a bulk string");
                                };
                                let max = if let Value::BulkString(max_bs) = &values[3] {
                                    String::from_utf8_lossy(max_bs).into_owned()
                                } else {
                                    return Err("ZRANGEBYLEX max must be a bulk string");
                                };
                                let mut limit: Option<(i64, i64)> = None;
                                if values.len() == 7 {
                                    if let Value::BulkString(lim_bs) = &values[4] {
                                        if String::from_utf8_lossy(lim_bs).to_uppercase() != "LIMIT"
                                        {
                                            return Err(
                                                "ZRANGEBYLEX only supports the LIMIT option",
                                            );
                                        }
                                    } else {
                                        return Err(
                                            "ZRANGEBYLEX option token must be a bulk string",
                                        );
                                    }
                                    let offset = if let Value::BulkString(off_bs) = &values[5] {
                                        String::from_utf8_lossy(off_bs).parse::<i64>().map_err(
                                            |_| "ZRANGEBYLEX LIMIT offset is not an integer",
                                        )?
                                    } else {
                                        return Err(
                                            "ZRANGEBYLEX LIMIT offset must be a bulk string",
                                        );
                                    };
                                    let count = if let Value::BulkString(cnt_bs) = &values[6] {
                                        String::from_utf8_lossy(cnt_bs).parse::<i64>().map_err(
                                            |_| "ZRANGEBYLEX LIMIT count is not an integer",
                                        )?
                                    } else {
                                        return Err(
                                            "ZRANGEBYLEX LIMIT count must be a bulk string",
                                        );
                                    };
                                    limit = Some((offset, count));
                                }
                                Ok(Command::ZRangeByLex {
                                    key,
                                    min,
                                    max,
                                    limit,
                                })
                            }
                            "ZREVRANGEBYLEX" => {
                                if values.len() != 4 && values.len() != 7 {
                                    return Err("ZREVRANGEBYLEX command expects key max min [LIMIT offset count]");
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("ZREVRANGEBYLEX key must be a bulk string");
                                };
                                let max = if let Value::BulkString(max_bs) = &values[2] {
                                    String::from_utf8_lossy(max_bs).into_owned()
                                } else {
                                    return Err("ZREVRANGEBYLEX max must be a bulk string");
                                };
                                let min = if let Value::BulkString(min_bs) = &values[3] {
                                    String::from_utf8_lossy(min_bs).into_owned()
                                } else {
                                    return Err("ZREVRANGEBYLEX min must be a bulk string");
                                };
                                let mut limit: Option<(i64, i64)> = None;
                                if values.len() == 7 {
                                    if let Value::BulkString(lim_bs) = &values[4] {
                                        if String::from_utf8_lossy(lim_bs).to_uppercase() != "LIMIT"
                                        {
                                            return Err(
                                                "ZREVRANGEBYLEX only supports the LIMIT option",
                                            );
                                        }
                                    } else {
                                        return Err(
                                            "ZREVRANGEBYLEX option token must be a bulk string",
                                        );
                                    }
                                    let offset = if let Value::BulkString(off_bs) = &values[5] {
                                        String::from_utf8_lossy(off_bs).parse::<i64>().map_err(
                                            |_| "ZREVRANGEBYLEX LIMIT offset is not an integer",
                                        )?
                                    } else {
                                        return Err(
                                            "ZREVRANGEBYLEX LIMIT offset must be a bulk string",
                                        );
                                    };
                                    let count = if let Value::BulkString(cnt_bs) = &values[6] {
                                        String::from_utf8_lossy(cnt_bs).parse::<i64>().map_err(
                                            |_| "ZREVRANGEBYLEX LIMIT count is not an integer",
                                        )?
                                    } else {
                                        return Err(
                                            "ZREVRANGEBYLEX LIMIT count must be a bulk string",
                                        );
                                    };
                                    limit = Some((offset, count));
                                }
                                Ok(Command::ZRevRangeByLex {
                                    key,
                                    max,
                                    min,
                                    limit,
                                })
                            }
                            "ZCOUNT" => {
                                if values.len() == 4 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZCOUNT key must be a bulk string");
                                    };
                                    let min = if let Value::BulkString(min_bs) = &values[2] {
                                        String::from_utf8_lossy(min_bs).into_owned()
                                    } else {
                                        return Err("ZCOUNT min must be a bulk string");
                                    };
                                    let max = if let Value::BulkString(max_bs) = &values[3] {
                                        String::from_utf8_lossy(max_bs).into_owned()
                                    } else {
                                        return Err("ZCOUNT max must be a bulk string");
                                    };
                                    Ok(Command::ZCount { key, min, max })
                                } else {
                                    Err("ZCOUNT command expects exactly three arguments (key min max)")
                                }
                            }
                            "ZLEXCOUNT" => {
                                if values.len() == 4 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZLEXCOUNT key must be a bulk string");
                                    };
                                    let min = if let Value::BulkString(min_bs) = &values[2] {
                                        String::from_utf8_lossy(min_bs).into_owned()
                                    } else {
                                        return Err("ZLEXCOUNT min must be a bulk string");
                                    };
                                    let max = if let Value::BulkString(max_bs) = &values[3] {
                                        String::from_utf8_lossy(max_bs).into_owned()
                                    } else {
                                        return Err("ZLEXCOUNT max must be a bulk string");
                                    };
                                    Ok(Command::ZLexCount { key, min, max })
                                } else {
                                    Err("ZLEXCOUNT command expects exactly three arguments (key min max)")
                                }
                            }
                            "ZPOPMIN" => {
                                if values.len() == 2 || values.len() == 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZPOPMIN key must be a bulk string");
                                    };
                                    let count = if values.len() == 3 {
                                        if let Value::BulkString(count_bs) = &values[2] {
                                            let c = String::from_utf8_lossy(count_bs)
                                                .parse::<usize>()
                                                .map_err(|_| {
                                                    "ZPOPMIN count must be an unsigned integer"
                                                })?;
                                            Some(c)
                                        } else {
                                            return Err("ZPOPMIN count must be a bulk string");
                                        }
                                    } else {
                                        None
                                    };
                                    Ok(Command::ZPopMin { key, count })
                                } else {
                                    Err("ZPOPMIN command expects one or two arguments (key [count])")
                                }
                            }
                            "ZPOPMAX" => {
                                if values.len() == 2 || values.len() == 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZPOPMAX key must be a bulk string");
                                    };
                                    let count = if values.len() == 3 {
                                        if let Value::BulkString(count_bs) = &values[2] {
                                            let c = String::from_utf8_lossy(count_bs)
                                                .parse::<usize>()
                                                .map_err(|_| {
                                                    "ZPOPMAX count must be an unsigned integer"
                                                })?;
                                            Some(c)
                                        } else {
                                            return Err("ZPOPMAX count must be a bulk string");
                                        }
                                    } else {
                                        None
                                    };
                                    Ok(Command::ZPopMax { key, count })
                                } else {
                                    Err("ZPOPMAX command expects one or two arguments (key [count])")
                                }
                            }
                            "ZREMRANGEBYRANK" => {
                                if values.len() == 4 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZREMRANGEBYRANK key must be a bulk string");
                                    };
                                    let start = if let Value::BulkString(start_bs) = &values[2] {
                                        String::from_utf8_lossy(start_bs).parse::<i64>().map_err(
                                            |_| "ZREMRANGEBYRANK start is not an integer",
                                        )?
                                    } else {
                                        return Err("ZREMRANGEBYRANK start must be a bulk string");
                                    };
                                    let stop = if let Value::BulkString(stop_bs) = &values[3] {
                                        String::from_utf8_lossy(stop_bs)
                                            .parse::<i64>()
                                            .map_err(|_| "ZREMRANGEBYRANK stop is not an integer")?
                                    } else {
                                        return Err("ZREMRANGEBYRANK stop must be a bulk string");
                                    };
                                    Ok(Command::ZRemRangeByRank { key, start, stop })
                                } else {
                                    Err("ZREMRANGEBYRANK command expects exactly three arguments (key start stop)")
                                }
                            }
                            "ZREMRANGEBYSCORE" => {
                                if values.len() == 4 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZREMRANGEBYSCORE key must be a bulk string");
                                    };
                                    let min = if let Value::BulkString(min_bs) = &values[2] {
                                        String::from_utf8_lossy(min_bs).into_owned()
                                    } else {
                                        return Err("ZREMRANGEBYSCORE min must be a bulk string");
                                    };
                                    let max = if let Value::BulkString(max_bs) = &values[3] {
                                        String::from_utf8_lossy(max_bs).into_owned()
                                    } else {
                                        return Err("ZREMRANGEBYSCORE max must be a bulk string");
                                    };
                                    Ok(Command::ZRemRangeByScore { key, min, max })
                                } else {
                                    Err("ZREMRANGEBYSCORE command expects exactly three arguments (key min max)")
                                }
                            }
                            "ZREMRANGEBYLEX" => {
                                if values.len() == 4 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZREMRANGEBYLEX key must be a bulk string");
                                    };
                                    let min = if let Value::BulkString(min_bs) = &values[2] {
                                        String::from_utf8_lossy(min_bs).into_owned()
                                    } else {
                                        return Err("ZREMRANGEBYLEX min must be a bulk string");
                                    };
                                    let max = if let Value::BulkString(max_bs) = &values[3] {
                                        String::from_utf8_lossy(max_bs).into_owned()
                                    } else {
                                        return Err("ZREMRANGEBYLEX max must be a bulk string");
                                    };
                                    Ok(Command::ZRemRangeByLex { key, min, max })
                                } else {
                                    Err("ZREMRANGEBYLEX command expects exactly three arguments (key min max)")
                                }
                            }
                            "ZRANDMEMBER" => {
                                if values.len() >= 2 && values.len() <= 4 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZRANDMEMBER key must be a bulk string");
                                    };
                                    let count = if values.len() >= 3 {
                                        if let Value::BulkString(count_bs) = &values[2] {
                                            let c = String::from_utf8_lossy(count_bs)
                                                .parse::<i64>()
                                                .map_err(|_| {
                                                    "ZRANDMEMBER count is not an integer"
                                                })?;
                                            Some(c)
                                        } else {
                                            return Err("ZRANDMEMBER count must be a bulk string");
                                        }
                                    } else {
                                        None
                                    };
                                    let with_scores = if values.len() == 4 {
                                        if let Value::BulkString(opt_bs) = &values[3] {
                                            if String::from_utf8_lossy(opt_bs).to_uppercase()
                                                == "WITHSCORES"
                                            {
                                                true
                                            } else {
                                                return Err("ZRANDMEMBER only supports the WITHSCORES option");
                                            }
                                        } else {
                                            return Err("ZRANDMEMBER option must be a bulk string");
                                        }
                                    } else {
                                        false
                                    };
                                    Ok(Command::ZRandMember {
                                        key,
                                        count,
                                        with_scores,
                                    })
                                } else {
                                    Err("ZRANDMEMBER command expects key [count [WITHSCORES]]")
                                }
                            }
                            "ZSCAN" => {
                                if values.len() >= 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("ZSCAN key must be a bulk string");
                                    };
                                    let cursor = if let Value::BulkString(cursor_bs) = &values[2] {
                                        String::from_utf8_lossy(cursor_bs).parse::<u64>().map_err(
                                            |_| "ZSCAN cursor must be an unsigned integer",
                                        )?
                                    } else {
                                        return Err("ZSCAN cursor must be a bulk string");
                                    };

                                    let mut pattern: Option<String> = None;
                                    let mut count: Option<usize> = None;
                                    let mut i = 3;
                                    while i < values.len() {
                                        let token = if let Value::BulkString(bs) = &values[i] {
                                            String::from_utf8_lossy(bs).to_uppercase()
                                        } else {
                                            return Err("ZSCAN option token must be a bulk string");
                                        };
                                        match token.as_str() {
                                            "MATCH" => {
                                                if i + 1 >= values.len() {
                                                    return Err(
                                                        "ZSCAN MATCH requires a pattern argument",
                                                    );
                                                }
                                                if let Value::BulkString(bs) = &values[i + 1] {
                                                    pattern = Some(
                                                        String::from_utf8_lossy(bs).into_owned(),
                                                    );
                                                } else {
                                                    return Err(
                                                        "ZSCAN MATCH pattern must be a bulk string",
                                                    );
                                                }
                                                i += 2;
                                            }
                                            "COUNT" => {
                                                if i + 1 >= values.len() {
                                                    return Err(
                                                        "ZSCAN COUNT requires a numeric argument",
                                                    );
                                                }
                                                if let Value::BulkString(bs) = &values[i + 1] {
                                                    let v = String::from_utf8_lossy(bs)
                                                        .parse::<usize>()
                                                        .map_err(|_| {
                                                            "ZSCAN COUNT must be an unsigned integer"
                                                        })?;
                                                    count = Some(v);
                                                } else {
                                                    return Err(
                                                        "ZSCAN COUNT value must be a bulk string",
                                                    );
                                                }
                                                i += 2;
                                            }
                                            _ => {
                                                return Err(
                                                    "ZSCAN only supports MATCH and COUNT options",
                                                )
                                            }
                                        }
                                    }
                                    Ok(Command::ZScan {
                                        key,
                                        cursor,
                                        pattern,
                                        count,
                                    })
                                } else {
                                    Err("ZSCAN command expects at least two arguments (key cursor)")
                                }
                            }

                            // Generic Commands
                            "TYPE" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Type { key })
                                    } else {
                                        Err("TYPE command argument must be a bulk string")
                                    }
                                } else {
                                    Err("TYPE command expects exactly one argument (key)")
                                }
                            }

                            "EXISTS" => {
                                if values.len() >= 2 {
                                    let mut keys = Vec::with_capacity(values.len() - 1);
                                    for val in &values[1..] {
                                        if let Value::BulkString(key_bs) = val {
                                            keys.push(String::from_utf8_lossy(key_bs).into_owned());
                                        } else {
                                            return Err(
                                                "EXISTS command arguments must be bulk strings",
                                            );
                                        }
                                    }
                                    Ok(Command::Exists { keys })
                                } else {
                                    Err("EXISTS command expects at least one argument (key ...)")
                                }
                            }
                            "TOUCH" => {
                                if values.len() >= 2 {
                                    let mut keys = Vec::with_capacity(values.len() - 1);
                                    for val in &values[1..] {
                                        if let Value::BulkString(key_bs) = val {
                                            keys.push(String::from_utf8_lossy(key_bs).into_owned());
                                        } else {
                                            return Err(
                                                "TOUCH command arguments must be bulk strings",
                                            );
                                        }
                                    }
                                    Ok(Command::Touch { keys })
                                } else {
                                    Err("TOUCH command expects at least one argument (key ...)")
                                }
                            }
                            "DEL" => {
                                if values.len() >= 2 {
                                    let mut keys = Vec::with_capacity(values.len() - 1);
                                    for val in &values[1..] {
                                        if let Value::BulkString(key_bs) = val {
                                            keys.push(String::from_utf8_lossy(key_bs).into_owned());
                                        } else {
                                            return Err(
                                                "DEL command arguments must be bulk strings",
                                            );
                                        }
                                    }
                                    Ok(Command::Del { keys })
                                } else {
                                    Err("DEL command expects at least one argument (key ...)")
                                }
                            }
                            "UNLINK" => {
                                if values.len() >= 2 {
                                    let mut keys = Vec::with_capacity(values.len() - 1);
                                    for val in &values[1..] {
                                        if let Value::BulkString(key_bs) = val {
                                            keys.push(String::from_utf8_lossy(key_bs).into_owned());
                                        } else {
                                            return Err(
                                                "UNLINK command arguments must be bulk strings",
                                            );
                                        }
                                    }
                                    Ok(Command::Unlink { keys })
                                } else {
                                    Err("UNLINK command expects at least one argument (key ...)")
                                }
                            }
                            "KEYS" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(pattern_bs) = &values[1] {
                                        let pattern =
                                            String::from_utf8_lossy(pattern_bs).into_owned();
                                        Ok(Command::Keys { pattern })
                                    } else {
                                        Err("KEYS command argument must be a bulk string")
                                    }
                                } else {
                                    Err("KEYS command expects exactly one argument (pattern)")
                                }
                            }
                            "RANDOMKEY" => {
                                if values.len() == 1 {
                                    Ok(Command::RandomKey {})
                                } else {
                                    Err("RANDOMKEY command expects no arguments")
                                }
                            }
                            "RENAME" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(new_key_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let new_key =
                                                String::from_utf8_lossy(new_key_bs).into_owned();
                                            Ok(Command::Rename { key, new_key })
                                        } else {
                                            Err("RENAME newkey must be a bulk string")
                                        }
                                    } else {
                                        Err("RENAME key must be a bulk string")
                                    }
                                } else {
                                    Err("RENAME command expects exactly two arguments (key and newkey)")
                                }
                            }
                            "RENAMENX" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(new_key_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let new_key =
                                                String::from_utf8_lossy(new_key_bs).into_owned();
                                            Ok(Command::RenameNx { key, new_key })
                                        } else {
                                            Err("RENAMENX newkey must be a bulk string")
                                        }
                                    } else {
                                        Err("RENAMENX key must be a bulk string")
                                    }
                                } else {
                                    Err("RENAMENX command expects exactly two arguments (key and newkey)")
                                }
                            }
                            "COPY" => {
                                if values.len() < 3 {
                                    return Err("COPY command expects at least two arguments (source destination)");
                                }

                                let source = if let Value::BulkString(src_bs) = &values[1] {
                                    String::from_utf8_lossy(src_bs).into_owned()
                                } else {
                                    return Err("COPY source must be a bulk string");
                                };

                                let destination = if let Value::BulkString(dst_bs) = &values[2] {
                                    String::from_utf8_lossy(dst_bs).into_owned()
                                } else {
                                    return Err("COPY destination must be a bulk string");
                                };

                                let mut destination_db = None;
                                let mut replace = false;
                                let mut i = 3;
                                while i < values.len() {
                                    let Value::BulkString(opt_bs) = &values[i] else {
                                        return Err("COPY option must be a bulk string");
                                    };
                                    let opt = String::from_utf8_lossy(opt_bs).to_uppercase();
                                    match opt.as_str() {
                                        "DB" => {
                                            if i + 1 >= values.len() {
                                                return Err("COPY DB option requires database index argument");
                                            }
                                            let Value::BulkString(db_bs) = &values[i + 1] else {
                                                return Err("COPY DB index must be a bulk string");
                                            };
                                            let db_str =
                                                String::from_utf8_lossy(db_bs).into_owned();
                                            let db = db_str.parse::<u32>().map_err(|_| {
                                                "COPY DB index must be a valid unsigned integer"
                                            })?;
                                            destination_db = Some(db);
                                            i += 2;
                                        }
                                        "REPLACE" => {
                                            replace = true;
                                            i += 1;
                                        }
                                        _ => {
                                            return Err("COPY supports only [DB destination-db] [REPLACE] options");
                                        }
                                    }
                                }

                                Ok(Command::Copy {
                                    source,
                                    destination,
                                    destination_db,
                                    replace,
                                })
                            }
                            "MOVE" => {
                                if values.len() == 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("MOVE key must be a bulk string");
                                    };
                                    let db = if let Value::BulkString(db_bs) = &values[2] {
                                        let db_str = String::from_utf8_lossy(db_bs).into_owned();
                                        db_str.parse::<u32>().map_err(|_| {
                                            "MOVE db must be a valid unsigned integer"
                                        })?
                                    } else {
                                        return Err("MOVE db must be a bulk string");
                                    };
                                    Ok(Command::Move { key, db })
                                } else {
                                    Err("MOVE command expects exactly two arguments (key and db)")
                                }
                            }

                            "EXPIRE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(seconds_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            if let Ok(seconds_str) = std::str::from_utf8(seconds_bs)
                                            {
                                                if let Ok(seconds) = seconds_str.parse::<u64>() {
                                                    Ok(Command::Expire { key, seconds })
                                                } else {
                                                    Err("EXPIRE seconds argument must be a valid unsigned integer")
                                                }
                                            } else {
                                                Err("EXPIRE seconds argument must be a valid unsigned integer")
                                            }
                                        } else {
                                            Err("EXPIRE seconds argument must be a bulk string")
                                        }
                                    } else {
                                        Err("EXPIRE key argument must be a bulk string")
                                    }
                                } else {
                                    Err("EXPIRE command expects exactly two arguments (key and seconds)")
                                }
                            }
                            "PEXPIRE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(ms_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            if let Ok(ms_str) = std::str::from_utf8(ms_bs) {
                                                if let Ok(milliseconds) = ms_str.parse::<u64>() {
                                                    Ok(Command::PExpire { key, milliseconds })
                                                } else {
                                                    Err("PEXPIRE milliseconds argument must be a valid unsigned integer")
                                                }
                                            } else {
                                                Err("PEXPIRE milliseconds argument must be a valid unsigned integer")
                                            }
                                        } else {
                                            Err("PEXPIRE milliseconds argument must be a bulk string")
                                        }
                                    } else {
                                        Err("PEXPIRE key argument must be a bulk string")
                                    }
                                } else {
                                    Err("PEXPIRE command expects exactly two arguments (key and milliseconds)")
                                }
                            }
                            "EXPIREAT" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(ts_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            if let Ok(ts_str) = std::str::from_utf8(ts_bs) {
                                                if let Ok(timestamp) = ts_str.parse::<u64>() {
                                                    Ok(Command::ExpireAt { key, timestamp })
                                                } else {
                                                    Err("EXPIREAT timestamp argument must be a valid unsigned integer")
                                                }
                                            } else {
                                                Err("EXPIREAT timestamp argument must be a valid unsigned integer")
                                            }
                                        } else {
                                            Err("EXPIREAT timestamp argument must be a bulk string")
                                        }
                                    } else {
                                        Err("EXPIREAT key argument must be a bulk string")
                                    }
                                } else {
                                    Err("EXPIREAT command expects exactly two arguments (key and timestamp)")
                                }
                            }
                            "PEXPIREAT" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(ts_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            if let Ok(ts_str) = std::str::from_utf8(ts_bs) {
                                                if let Ok(milliseconds_timestamp) =
                                                    ts_str.parse::<u64>()
                                                {
                                                    Ok(Command::PExpireAt {
                                                        key,
                                                        milliseconds_timestamp,
                                                    })
                                                } else {
                                                    Err("PEXPIREAT timestamp argument must be a valid unsigned integer")
                                                }
                                            } else {
                                                Err("PEXPIREAT timestamp argument must be a valid unsigned integer")
                                            }
                                        } else {
                                            Err("PEXPIREAT timestamp argument must be a bulk string")
                                        }
                                    } else {
                                        Err("PEXPIREAT key argument must be a bulk string")
                                    }
                                } else {
                                    Err("PEXPIREAT command expects exactly two arguments (key and milliseconds-timestamp)")
                                }
                            }
                            "TTL" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Ttl { key })
                                    } else {
                                        Err("TTL command argument must be a bulk string")
                                    }
                                } else {
                                    Err("TTL command expects exactly one argument (key)")
                                }
                            }
                            "PTTL" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Pttl { key })
                                    } else {
                                        Err("PTTL command argument must be a bulk string")
                                    }
                                } else {
                                    Err("PTTL command expects exactly one argument (key)")
                                }
                            }
                            "EXPIRETIME" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::ExpireTime { key })
                                    } else {
                                        Err("EXPIRETIME key argument must be a bulk string")
                                    }
                                } else {
                                    Err("EXPIRETIME command expects exactly one argument (key)")
                                }
                            }
                            "PEXPIRETIME" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::PExpireTime { key })
                                    } else {
                                        Err("PEXPIRETIME key argument must be a bulk string")
                                    }
                                } else {
                                    Err("PEXPIRETIME command expects exactly one argument (key)")
                                }
                            }
                            "PERSIST" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Persist { key })
                                    } else {
                                        Err("PERSIST command argument must be a bulk string")
                                    }
                                } else {
                                    Err("PERSIST command expects exactly one argument (key)")
                                }
                            }
                            "OBJECT" => {
                                if values.len() == 3 {
                                    let sub = if let Value::BulkString(sub_bs) = &values[1] {
                                        String::from_utf8_lossy(sub_bs).to_uppercase()
                                    } else {
                                        return Err("OBJECT subcommand must be a bulk string");
                                    };
                                    let key = if let Value::BulkString(key_bs) = &values[2] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("OBJECT key must be a bulk string");
                                    };
                                    match sub.as_str() {
                                        "ENCODING" => Ok(Command::ObjectEncoding { key }),
                                        "REFCOUNT" => Ok(Command::ObjectRefCount { key }),
                                        "IDLETIME" => Ok(Command::ObjectIdleTime { key }),
                                        _ => Err("OBJECT supports ENCODING|REFCOUNT|IDLETIME"),
                                    }
                                } else {
                                    Err("OBJECT command expects exactly two arguments (subcommand and key)")
                                }
                            }
                            "DBSIZE" => {
                                if values.len() == 1 {
                                    Ok(Command::DbSize {})
                                } else {
                                    Err("DBSIZE command expects no arguments")
                                }
                            }
                            "FLUSHDB" => {
                                if values.len() == 1 {
                                    Ok(Command::FlushDb {})
                                } else {
                                    Err("FLUSHDB command expects no arguments")
                                }
                            }
                            "FLUSHALL" => {
                                if values.len() == 1 {
                                    Ok(Command::FlushAll {})
                                } else {
                                    Err("FLUSHALL command expects no arguments")
                                }
                            }
                            "SELECT" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(db_bs) = &values[1] {
                                        if let Ok(db_str) = std::str::from_utf8(db_bs) {
                                            if let Ok(db) = db_str.parse::<u32>() {
                                                Ok(Command::Select { db })
                                            } else {
                                                Err("SELECT db must be an unsigned integer")
                                            }
                                        } else {
                                            Err("SELECT db must be an unsigned integer")
                                        }
                                    } else {
                                        Err("SELECT db argument must be a bulk string")
                                    }
                                } else {
                                    Err("SELECT command expects exactly one argument (db)")
                                }
                            }
                            "SCAN" => {
                                if values.len() >= 2 {
                                    let cursor = if let Value::BulkString(cursor_bs) = &values[1] {
                                        let cursor_str =
                                            String::from_utf8_lossy(cursor_bs).into_owned();
                                        cursor_str.parse::<u64>().map_err(|_| {
                                            "SCAN cursor must be an unsigned integer"
                                        })?
                                    } else {
                                        return Err("SCAN cursor must be a bulk string");
                                    };

                                    let mut pattern: Option<String> = None;
                                    let mut count: Option<usize> = None;
                                    let mut i = 2;
                                    while i < values.len() {
                                        let token = if let Value::BulkString(bs) = &values[i] {
                                            String::from_utf8_lossy(bs).to_uppercase()
                                        } else {
                                            return Err("SCAN option token must be a bulk string");
                                        };
                                        match token.as_str() {
                                            "MATCH" => {
                                                if i + 1 >= values.len() {
                                                    return Err(
                                                        "SCAN MATCH requires a pattern argument",
                                                    );
                                                }
                                                if let Value::BulkString(bs) = &values[i + 1] {
                                                    pattern = Some(
                                                        String::from_utf8_lossy(bs).into_owned(),
                                                    );
                                                } else {
                                                    return Err(
                                                        "SCAN MATCH pattern must be a bulk string",
                                                    );
                                                }
                                                i += 2;
                                            }
                                            "COUNT" => {
                                                if i + 1 >= values.len() {
                                                    return Err(
                                                        "SCAN COUNT requires a numeric argument",
                                                    );
                                                }
                                                if let Value::BulkString(bs) = &values[i + 1] {
                                                    let v = String::from_utf8_lossy(bs)
                                                        .parse::<usize>()
                                                        .map_err(|_| {
                                                            "SCAN COUNT must be an unsigned integer"
                                                        })?;
                                                    count = Some(v);
                                                } else {
                                                    return Err(
                                                        "SCAN COUNT value must be a bulk string",
                                                    );
                                                }
                                                i += 2;
                                            }
                                            _ => {
                                                return Err(
                                                    "SCAN only supports MATCH and COUNT options",
                                                )
                                            }
                                        }
                                    }
                                    Ok(Command::Scan {
                                        cursor,
                                        pattern,
                                        count,
                                    })
                                } else {
                                    Err("SCAN command expects at least one argument (cursor)")
                                }
                            }
                            "WAIT" => {
                                if values.len() == 3 {
                                    let numreplicas = if let Value::BulkString(n_bs) = &values[1] {
                                        let n_str = String::from_utf8_lossy(n_bs).into_owned();
                                        n_str.parse::<u64>().map_err(|_| {
                                            "WAIT numreplicas must be a valid unsigned integer"
                                        })?
                                    } else {
                                        return Err("WAIT numreplicas must be a bulk string");
                                    };
                                    let timeout_ms = if let Value::BulkString(t_bs) = &values[2] {
                                        let t_str = String::from_utf8_lossy(t_bs).into_owned();
                                        t_str.parse::<u64>().map_err(|_| {
                                            "WAIT timeout must be a valid unsigned integer"
                                        })?
                                    } else {
                                        return Err("WAIT timeout must be a bulk string");
                                    };
                                    Ok(Command::Wait {
                                        numreplicas,
                                        timeout_ms,
                                    })
                                } else {
                                    Err("WAIT command expects exactly two arguments (numreplicas timeout)")
                                }
                            }
                            // String commands
                            "SET" => {
                                let mut key: Option<String> = None;
                                let mut value: Option<Vec<u8>> = None;
                                let mut expire_option: Option<SetExpireOption> = None;
                                let mut set_option: Option<SetOption> = None;

                                let mut i = 1;
                                while i < values.len() {
                                    match i {
                                        1 => {
                                            if let Value::BulkString(key_bs) = &values[i] {
                                                key = Some(
                                                    String::from_utf8_lossy(key_bs).into_owned(),
                                                );
                                            } else {
                                                return Err("SET key must be a bulk string");
                                            }
                                        }
                                        2 => {
                                            if let Value::BulkString(value_bs) = &values[i] {
                                                // Binary-safe: store raw value bytes verbatim.
                                                value = Some(value_bs.clone());
                                            } else {
                                                return Err("SET value must be a bulk string");
                                            }
                                        }
                                        _ => {
                                            if let Value::BulkString(option_bs) = &values[i] {
                                                let option_str = String::from_utf8_lossy(option_bs)
                                                    .to_uppercase();
                                                match option_str.as_str() {
                                                    "EX" | "PX" => {
                                                        if i + 1 < values.len() {
                                                            if let Value::BulkString(time_bs) =
                                                                &values[i + 1]
                                                            {
                                                                if let Ok(time_val) =
                                                                    String::from_utf8_lossy(time_bs)
                                                                        .parse::<u64>()
                                                                {
                                                                    match option_str.as_str() {
                                                                        "EX" => {
                                                                            expire_option = Some(
                                                                                SetExpireOption::EX(
                                                                                    time_val,
                                                                                ),
                                                                            )
                                                                        }
                                                                        "PX" => {
                                                                            expire_option = Some(
                                                                                SetExpireOption::PX(
                                                                                    time_val,
                                                                                ),
                                                                            )
                                                                        }
                                                                        _ => unreachable!(), // Should not happen
                                                                    }
                                                                    i += 1; // consume next argument (time value)
                                                                } else {
                                                                    return Err("SET EX/PX value is not a valid number");
                                                                }
                                                            } else {
                                                                return Err("SET EX/PX value must be a bulk string");
                                                            }
                                                        } else {
                                                            return Err("SET EX/PX option requires a time argument");
                                                        }
                                                    }
                                                    "NX" => set_option = Some(SetOption::NX),
                                                    "XX" => set_option = Some(SetOption::XX),
                                                    _ => return Err(
                                                        "SET invalid option, use EX, PX, NX, XX",
                                                    ),
                                                }
                                            } else {
                                                return Err("SET option must be a bulk string");
                                            }
                                        }
                                    }
                                    i += 1;
                                }

                                if let Some(key_val) = key {
                                    if let Some(value_val) = value {
                                        Ok(Command::Set {
                                            key: key_val,
                                            value: value_val,
                                            expire_option,
                                            set_option,
                                        })
                                    } else {
                                        Err("SET command requires a value argument")
                                    }
                                } else {
                                    Err("SET command requires a key argument")
                                }
                            }
                            "SETNX" => {
                                if values.len() == 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("SETNX key must be a bulk string");
                                    };
                                    let value = if let Value::BulkString(value_bs) = &values[2] {
                                        value_bs.clone()
                                    } else {
                                        return Err("SETNX value must be a bulk string");
                                    };
                                    Ok(Command::SetNx { key, value })
                                } else {
                                    Err("SETNX command expects exactly two arguments (key and value)")
                                }
                            }
                            "SETEX" => {
                                if values.len() == 4 {
                                    let key = if let Value::BulkString(bs) = &values[1] {
                                        String::from_utf8_lossy(bs).into_owned()
                                    } else {
                                        return Err("SETEX key must be a bulk string");
                                    };
                                    let seconds = if let Value::BulkString(bs) = &values[2] {
                                        String::from_utf8_lossy(bs).parse::<u64>().map_err(
                                            |_| "SETEX seconds must be an unsigned integer",
                                        )?
                                    } else {
                                        return Err("SETEX seconds must be a bulk string");
                                    };
                                    let value = if let Value::BulkString(bs) = &values[3] {
                                        bs.clone()
                                    } else {
                                        return Err("SETEX value must be a bulk string");
                                    };

                                    Ok(Command::SetEx {
                                        key,
                                        seconds,
                                        value,
                                    })
                                } else {
                                    Err("SETEX command expects exactly three arguments (key, seconds, value)")
                                }
                            }
                            "PSETEX" => {
                                if values.len() == 4 {
                                    let key = if let Value::BulkString(bs) = &values[1] {
                                        String::from_utf8_lossy(bs).into_owned()
                                    } else {
                                        return Err("PSETEX key must be a bulk string");
                                    };
                                    let milliseconds = if let Value::BulkString(bs) = &values[2] {
                                        String::from_utf8_lossy(bs).parse::<u64>().map_err(
                                            |_| "PSETEX milliseconds must be an unsigned integer",
                                        )?
                                    } else {
                                        return Err("PSETEX milliseconds must be a bulk string");
                                    };
                                    let value = if let Value::BulkString(bs) = &values[3] {
                                        bs.clone()
                                    } else {
                                        return Err("PSETEX value must be a bulk string");
                                    };

                                    Ok(Command::PSetEx {
                                        key,
                                        milliseconds,
                                        value,
                                    })
                                } else {
                                    Err("PSETEX command expects exactly three arguments (key, milliseconds, value)")
                                }
                            }
                            "GET" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Get { key })
                                    } else {
                                        Err("GET command argument must be a bulk string")
                                    }
                                } else {
                                    Err("GET command expects exactly one argument (key)")
                                }
                            }
                            "GETDEL" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::GetDel { key })
                                    } else {
                                        Err("GETDEL command argument must be a bulk string")
                                    }
                                } else {
                                    Err("GETDEL command expects exactly one argument (key)")
                                }
                            }
                            "GETEX" => {
                                if values.len() >= 2 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("GETEX key must be a bulk string");
                                    };

                                    if values.len() == 2 {
                                        Ok(Command::GetEx { key, option: None })
                                    } else {
                                        let opt = if let Value::BulkString(opt_bs) = &values[2] {
                                            String::from_utf8_lossy(opt_bs).to_uppercase()
                                        } else {
                                            return Err("GETEX option must be a bulk string");
                                        };

                                        let option = match opt.as_str() {
                                            "PERSIST" => {
                                                if values.len() != 3 {
                                                    return Err("GETEX PERSIST takes no additional argument");
                                                }
                                                GetExOption::Persist
                                            }
                                            "EX" | "PX" | "EXAT" | "PXAT" => {
                                                if values.len() != 4 {
                                                    return Err("GETEX EX|PX|EXAT|PXAT requires one numeric argument");
                                                }
                                                let num = if let Value::BulkString(n_bs) =
                                                    &values[3]
                                                {
                                                    String::from_utf8_lossy(n_bs)
                                                        .parse::<u64>()
                                                        .map_err(|_| "GETEX numeric argument must be an unsigned integer")?
                                                } else {
                                                    return Err("GETEX numeric argument must be a bulk string");
                                                };
                                                match opt.as_str() {
                                                    "EX" => GetExOption::EX(num),
                                                    "PX" => GetExOption::PX(num),
                                                    "EXAT" => GetExOption::EXAT(num),
                                                    "PXAT" => GetExOption::PXAT(num),
                                                    _ => unreachable!(),
                                                }
                                            }
                                            _ => {
                                                return Err(
                                                    "GETEX supports EX|PX|EXAT|PXAT|PERSIST",
                                                );
                                            }
                                        };
                                        Ok(Command::GetEx {
                                            key,
                                            option: Some(option),
                                        })
                                    }
                                } else {
                                    Err("GETEX command expects at least one argument (key)")
                                }
                            }
                            "APPEND" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(value_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let value = value_bs.clone();
                                            Ok(Command::Append { key, value })
                                        } else {
                                            Err("APPEND value must be a bulk string")
                                        }
                                    } else {
                                        Err("APPEND key must be a bulk string")
                                    }
                                } else {
                                    Err("APPEND command expects exactly two arguments (key and value)")
                                }
                            }
                            "STRLEN" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::StrLen { key })
                                    } else {
                                        Err("STRLEN command argument must be a bulk string")
                                    }
                                } else {
                                    Err("STRLEN command expects exactly one argument (key)")
                                }
                            }
                            "GETRANGE" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(start_bs) = &values[2] {
                                            if let Value::BulkString(end_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let start = String::from_utf8_lossy(start_bs)
                                                    .into_owned()
                                                    .parse::<isize>()
                                                    .map_err(|_| {
                                                        "GETRANGE start is not an integer"
                                                    })?;
                                                let end = String::from_utf8_lossy(end_bs)
                                                    .into_owned()
                                                    .parse::<isize>()
                                                    .map_err(|_| {
                                                        "GETRANGE end is not an integer"
                                                    })?;
                                                Ok(Command::GetRange { key, start, end })
                                            } else {
                                                Err("GETRANGE end must be a bulk string")
                                            }
                                        } else {
                                            Err("GETRANGE start must be a bulk string")
                                        }
                                    } else {
                                        Err("GETRANGE key must be a bulk string")
                                    }
                                } else {
                                    Err("GETRANGE command expects exactly three arguments (key, start, end)")
                                }
                            }
                            "SETRANGE" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(offset_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let offset = String::from_utf8_lossy(offset_bs)
                                                    .into_owned()
                                                    .parse::<usize>()
                                                    .map_err(|_| {
                                                        "SETRANGE offset is not an unsigned integer"
                                                    })?;
                                                let value = value_bs.clone();
                                                Ok(Command::SetRange { key, offset, value })
                                            } else {
                                                Err("SETRANGE value must be a bulk string")
                                            }
                                        } else {
                                            Err("SETRANGE offset must be a bulk string")
                                        }
                                    } else {
                                        Err("SETRANGE key must be a bulk string")
                                    }
                                } else {
                                    Err("SETRANGE command expects exactly three arguments (key, offset, value)")
                                }
                            }
                            "GETSET" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(value_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let value = value_bs.clone();
                                            Ok(Command::GetSet { key, value })
                                        } else {
                                            Err("GETSET value must be a bulk string")
                                        }
                                    } else {
                                        Err("GETSET key must be a bulk string")
                                    }
                                } else {
                                    Err("GETSET command expects exactly two arguments (key and value)")
                                }
                            }
                            "MGET" => {
                                if values.len() >= 2 {
                                    let mut keys: Vec<String> =
                                        Vec::with_capacity(values.len() - 1);
                                    for val in &values[1..] {
                                        if let Value::BulkString(key_bs) = val {
                                            keys.push(String::from_utf8_lossy(key_bs).into_owned());
                                        } else {
                                            return Err("MGET keys must be bulk strings");
                                        }
                                    }
                                    Ok(Command::MGet { keys })
                                } else {
                                    Err("MGET command expects at least one key argument")
                                }
                            }
                            "MSET" => {
                                if values.len() >= 3 && values.len() % 2 == 1 {
                                    let mut key_values: Vec<(String, Vec<u8>)> = Vec::new();
                                    let mut i = 1;
                                    while i < values.len() {
                                        if let (
                                            Value::BulkString(key_bs),
                                            Value::BulkString(value_bs),
                                        ) = (&values[i], &values[i + 1])
                                        {
                                            key_values.push((
                                                String::from_utf8_lossy(key_bs).into_owned(),
                                                value_bs.clone(),
                                            ));
                                            i += 2;
                                        } else {
                                            return Err("MSET command expects key value pairs as bulk strings");
                                        }
                                    }
                                    Ok(Command::MSet { key_values })
                                } else {
                                    Err("MSET command expects key value pairs, odd number of arguments including command name")
                                }
                            }
                            "MSETNX" => {
                                if values.len() >= 3 && values.len() % 2 == 1 {
                                    let mut key_values: Vec<(String, Vec<u8>)> = Vec::new();
                                    let mut i = 1;
                                    while i < values.len() {
                                        if let (
                                            Value::BulkString(key_bs),
                                            Value::BulkString(value_bs),
                                        ) = (&values[i], &values[i + 1])
                                        {
                                            key_values.push((
                                                String::from_utf8_lossy(key_bs).into_owned(),
                                                value_bs.clone(),
                                            ));
                                            i += 2;
                                        } else {
                                            return Err("MSETNX command expects key value pairs as bulk strings");
                                        }
                                    }
                                    Ok(Command::MSetNx { key_values })
                                } else {
                                    Err("MSETNX command expects key value pairs, odd number of arguments including command name")
                                }
                            }
                            "SADD" => {
                                if values.len() >= 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("SADD key must be a bulk string");
                                    };
                                    let mut members = Vec::with_capacity(values.len() - 2);
                                    for v in &values[2..] {
                                        if let Value::BulkString(m_bs) = v {
                                            members.push(m_bs.clone());
                                        } else {
                                            return Err("SADD members must be bulk strings");
                                        }
                                    }
                                    Ok(Command::SAdd { key, members })
                                } else {
                                    Err("SADD command expects at least two arguments (key member [member ...])")
                                }
                            }
                            "SREM" => {
                                if values.len() >= 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("SREM key must be a bulk string");
                                    };
                                    let mut members = Vec::with_capacity(values.len() - 2);
                                    for v in &values[2..] {
                                        if let Value::BulkString(m_bs) = v {
                                            members.push(m_bs.clone());
                                        } else {
                                            return Err("SREM members must be bulk strings");
                                        }
                                    }
                                    Ok(Command::SRem { key, members })
                                } else {
                                    Err("SREM command expects at least two arguments (key member [member ...])")
                                }
                            }
                            "SMEMBERS" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::SMembers { key })
                                    } else {
                                        Err("SMEMBERS key must be a bulk string")
                                    }
                                } else {
                                    Err("SMEMBERS command expects exactly one argument (key)")
                                }
                            }
                            "SISMEMBER" => {
                                if values.len() == 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("SISMEMBER key must be a bulk string");
                                    };
                                    let member = if let Value::BulkString(m_bs) = &values[2] {
                                        m_bs.clone()
                                    } else {
                                        return Err("SISMEMBER member must be a bulk string");
                                    };
                                    Ok(Command::SIsMember { key, member })
                                } else {
                                    Err("SISMEMBER command expects exactly two arguments (key member)")
                                }
                            }
                            "SCARD" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::SCard { key })
                                    } else {
                                        Err("SCARD key must be a bulk string")
                                    }
                                } else {
                                    Err("SCARD command expects exactly one argument (key)")
                                }
                            }
                            "SPOP" => {
                                if values.len() == 2 || values.len() == 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("SPOP key must be a bulk string");
                                    };

                                    let count = if values.len() == 3 {
                                        if let Value::BulkString(count_bs) = &values[2] {
                                            let c = String::from_utf8_lossy(count_bs)
                                                .parse::<usize>()
                                                .map_err(|_| {
                                                    "SPOP count must be an unsigned integer"
                                                })?;
                                            Some(c)
                                        } else {
                                            return Err("SPOP count must be a bulk string");
                                        }
                                    } else {
                                        None
                                    };

                                    Ok(Command::SPop { key, count })
                                } else {
                                    Err("SPOP command expects one or two arguments (key [count])")
                                }
                            }
                            "SRANDMEMBER" => {
                                if values.len() == 2 || values.len() == 3 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("SRANDMEMBER key must be a bulk string");
                                    };

                                    let count = if values.len() == 3 {
                                        if let Value::BulkString(count_bs) = &values[2] {
                                            let c = String::from_utf8_lossy(count_bs)
                                                .parse::<isize>()
                                                .map_err(|_| {
                                                    "SRANDMEMBER count must be an integer"
                                                })?;
                                            Some(c)
                                        } else {
                                            return Err("SRANDMEMBER count must be a bulk string");
                                        }
                                    } else {
                                        None
                                    };

                                    Ok(Command::SRandMember { key, count })
                                } else {
                                    Err("SRANDMEMBER command expects one or two arguments (key [count])")
                                }
                            }

                            "INCR" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Incr { key })
                                    } else {
                                        Err("INCR command argument must be a bulk string")
                                    }
                                } else {
                                    Err("INCR command expects exactly one argument (key)")
                                }
                            }
                            "DECR" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::Decr { key })
                                    } else {
                                        Err("DECR command argument must be a bulk string")
                                    }
                                } else {
                                    Err("DECR command expects exactly one argument (key)")
                                }
                            }

                            "INCRBY" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(increment_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let increment = String::from_utf8_lossy(increment_bs)
                                                .into_owned()
                                                .parse::<i64>()
                                                .map_err(|_| {
                                                    "INCRBY increment is not an integer"
                                                })?;
                                            Ok(Command::IncrBy { key, increment })
                                        } else {
                                            Err("INCRBY increment must be a bulk string")
                                        }
                                    } else {
                                        Err("INCRBY key must be a bulk string")
                                    }
                                } else {
                                    Err("INCRBY command expects exactly two arguments (key and increment)")
                                }
                            }
                            "DECRBY" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(decrement_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let decrement = String::from_utf8_lossy(decrement_bs)
                                                .into_owned()
                                                .parse::<i64>()
                                                .map_err(|_| {
                                                    "DECRBY decrement is not an integer"
                                                })?;
                                            Ok(Command::DecrBy { key, decrement })
                                        } else {
                                            Err("DECRBY decrement must be a bulk string")
                                        }
                                    } else {
                                        Err("DECRBY key must be a bulk string")
                                    }
                                } else {
                                    Err("DECRBY command expects exactly two arguments (key and decrement)")
                                }
                            }

                            "INCRBYFLOAT" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(increment_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let increment = String::from_utf8_lossy(increment_bs)
                                                .into_owned()
                                                .parse::<f64>()
                                                .map_err(|_| {
                                                    "INCRBYFLOAT increment is not a float"
                                                })?;
                                            Ok(Command::IncrByFloat { key, increment })
                                        } else {
                                            Err("INCRBYFLOAT increment must be a bulk string")
                                        }
                                    } else {
                                        Err("INCRBYFLOAT key must be a bulk string")
                                    }
                                } else {
                                    Err("INCRBYFLOAT command expects exactly two arguments (key and increment)")
                                }
                            }

                            // Hash Commands
                            "HSET" => {
                                if values.len() >= 4 && (values.len() - 2).is_multiple_of(2) {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut field_values: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                                        let mut i = 2;
                                        while i < values.len() {
                                            if let (
                                                Value::BulkString(field_bs),
                                                Value::BulkString(value_bs),
                                            ) = (&values[i], &values[i + 1])
                                            {
                                                field_values
                                                    .push((field_bs.clone(), value_bs.clone()));
                                                i += 2;
                                            } else {
                                                return Err("HSET command expects field value pairs as bulk strings after key");
                                            }
                                        }
                                        Ok(Command::HSet { key, field_values })
                                    } else {
                                        Err("HSET command key must be a bulk string")
                                    }
                                } else {
                                    Err("HSET command expects at least one field-value pair after key, even number of arguments after command name and key")
                                }
                            }

                            "HGET" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(field_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let field = field_bs.clone();
                                            Ok(Command::HGet { key, field })
                                        } else {
                                            Err("HGET command field must be a bulk string")
                                        }
                                    } else {
                                        Err("HGET command key must be a bulk string")
                                    }
                                } else {
                                    Err("HGET command expects exactly two arguments (key, field)")
                                }
                            }

                            "HDEL" => {
                                if values.len() >= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut fields: Vec<Vec<u8>> =
                                            Vec::with_capacity(values.len() - 2);
                                        for val in &values[2..] {
                                            if let Value::BulkString(field_bs) = val {
                                                fields.push(field_bs.clone());
                                            } else {
                                                return Err("HDEL fields must be bulk strings");
                                            }
                                        }
                                        Ok(Command::HDel { key, fields })
                                    } else {
                                        Err("HDEL command key must be a bulk string")
                                    }
                                } else {
                                    Err("HDEL command expects at least one field argument after key")
                                }
                            }

                            "HEXISTS" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(field_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let field = field_bs.clone();
                                            Ok(Command::HExists { key, field })
                                        } else {
                                            Err("HEXISTS command field must be a bulk string")
                                        }
                                    } else {
                                        Err("HEXISTS command key must be a bulk string")
                                    }
                                } else {
                                    Err("HEXISTS command expects exactly two arguments (key and field)")
                                }
                            }

                            "HGETALL" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::HGetAll { key })
                                    } else {
                                        Err("HGETALL command key must be a bulk string")
                                    }
                                } else {
                                    Err("HGETALL command expects exactly one argument (key)")
                                }
                            }

                            "HINCRBY" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(field_bs) = &values[2] {
                                            if let Value::BulkString(increment_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let field = field_bs.clone();
                                                let increment =
                                                    String::from_utf8_lossy(increment_bs)
                                                        .into_owned()
                                                        .parse::<i64>()
                                                        .map_err(|_| {
                                                            "HINCRBY increment is not an integer"
                                                        })?;
                                                Ok(Command::HIncrBy {
                                                    key,
                                                    field,
                                                    increment,
                                                })
                                            } else {
                                                Err("HINCRBY increment must be a bulk string")
                                            }
                                        } else {
                                            Err("HINCRBY field must be a bulk string")
                                        }
                                    } else {
                                        Err("HINCRBY key must be a bulk string")
                                    }
                                } else {
                                    Err("HINCRBY command expects exactly three arguments (key, field, increment)")
                                }
                            }

                            "HINCRBYFLOAT" => {
                                if values.len() >= 4 && (values.len() - 2).is_multiple_of(2) {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut key_field_increment: Vec<(Vec<u8>, f64)> =
                                            Vec::new();
                                        let mut i = 2;
                                        while i < values.len() {
                                            if let (
                                                Value::BulkString(field_bs),
                                                Value::BulkString(increment_bs),
                                            ) = (&values[i], &values[i + 1])
                                            {
                                                let field = field_bs.clone();
                                                let increment =
                                                    String::from_utf8_lossy(increment_bs)
                                                        .into_owned()
                                                        .parse::<f64>()
                                                        .map_err(|_| {
                                                            "HINCRBYFLOAT increment is not a float"
                                                        })?;
                                                key_field_increment.push((field, increment));
                                                i += 2;
                                            } else {
                                                return Err("HINCRBYFLOAT command expects field increment pairs as bulk strings after key");
                                            }
                                        }
                                        Ok(Command::HIncrByFloat {
                                            key,
                                            key_field_increment,
                                        })
                                    } else {
                                        Err("HINCRBYFLOAT command key must be a bulk string")
                                    }
                                } else {
                                    Err("HINCRBYFLOAT command expects at least one field-increment pair after key, even number of arguments after command name and key")
                                }
                            }

                            "HKEYS" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::HKeys { key })
                                    } else {
                                        Err("HKEYS command key must be a bulk string")
                                    }
                                } else {
                                    Err("HKEYS command expects exactly one argument (key)")
                                }
                            }

                            "HLEN" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::HLen { key })
                                    } else {
                                        Err("HLEN command key must be a bulk string")
                                    }
                                } else {
                                    Err("HLEN command expects exactly one argument (key)")
                                }
                            }

                            "HMGET" => {
                                if values.len() >= 2 {
                                    // HMGET can have one or more fields after key
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut fields: Vec<Vec<u8>> = Vec::new();
                                        for value in values.iter().skip(2) {
                                            if let Value::BulkString(field_bs) = value {
                                                fields.push(field_bs.clone());
                                            } else {
                                                return Err("HMGET fields must be bulk strings");
                                            }
                                        }
                                        Ok(Command::HMGet { key, fields })
                                    } else {
                                        Err("HMGET command key must be a bulk string")
                                    }
                                } else {
                                    Err("HMGET command expects at least one argument (key)")
                                }
                            }

                            "HMSET" => {
                                if values.len() >= 4 && (values.len() - 2).is_multiple_of(2) {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut field_values: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
                                        let mut i = 2;
                                        while i < values.len() {
                                            if let (
                                                Value::BulkString(field_bs),
                                                Value::BulkString(value_bs),
                                            ) = (&values[i], &values[i + 1])
                                            {
                                                field_values
                                                    .push((field_bs.clone(), value_bs.clone()));
                                                i += 2;
                                            } else {
                                                return Err("HMSET command expects field value pairs as bulk strings after key");
                                            }
                                        }
                                        Ok(Command::HMSet { key, field_values })
                                    } else {
                                        Err("HMSET command key must be a bulk string")
                                    }
                                } else {
                                    Err("HMSET command expects at least one field-value pair after key, even number of arguments after command name and key")
                                }
                            }

                            "HSETNX" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(field_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let field = field_bs.clone();
                                                let value = value_bs.clone();
                                                Ok(Command::HSetNx { key, field, value })
                                            } else {
                                                Err("HSETNX value must be a bulk string")
                                            }
                                        } else {
                                            Err("HSETNX field must be a bulk string")
                                        }
                                    } else {
                                        Err("HSETNX key must be a bulk string")
                                    }
                                } else {
                                    Err("HSETNX command expects exactly three arguments (key, field, value)")
                                }
                            }

                            "HVALS" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::HVals { key })
                                    } else {
                                        Err("HVALS command key must be a bulk string")
                                    }
                                } else {
                                    Err("HVALS command expects exactly one argument (key)")
                                }
                            }

                            // List commands
                            "LPUSH" => {
                                if values.len() >= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut values_list: Vec<Vec<u8>> =
                                            Vec::with_capacity(values.len() - 2);
                                        for val in &values[2..] {
                                            if let Value::BulkString(value_bs) = val {
                                                values_list.push(value_bs.clone());
                                            } else {
                                                return Err("LPUSH values must be bulk strings");
                                            }
                                        }
                                        Ok(Command::ListLPush {
                                            key,
                                            values: values_list,
                                        })
                                    } else {
                                        Err("LPUSH command key must be a bulk string")
                                    }
                                } else {
                                    Err("LPUSH command expects at least one value argument after key")
                                }
                            }

                            "RPUSH" => {
                                if values.len() >= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut values_list: Vec<Vec<u8>> =
                                            Vec::with_capacity(values.len() - 2);
                                        for val in &values[2..] {
                                            if let Value::BulkString(value_bs) = val {
                                                values_list.push(value_bs.clone());
                                            } else {
                                                return Err("RPUSH values must be bulk strings");
                                            }
                                        }
                                        Ok(Command::ListRPush {
                                            key,
                                            values: values_list,
                                        })
                                    } else {
                                        Err("RPUSH command key must be a bulk string")
                                    }
                                } else {
                                    Err("RPUSH command expects at least one value argument after key")
                                }
                            }

                            "LPOP" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::LPop { key })
                                    } else {
                                        Err("LPOP command key must be a bulk string")
                                    }
                                } else {
                                    Err("LPOP command expects exactly one argument (key)")
                                }
                            }

                            "RPOP" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::RPop { key })
                                    } else {
                                        Err("RPOP command key must be a bulk string")
                                    }
                                } else {
                                    Err("RPOP command expects exactly one argument (key)")
                                }
                            }

                            "LLEN" => {
                                if values.len() == 2 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        Ok(Command::LLen { key })
                                    } else {
                                        Err("LLEN command key must be a bulk string")
                                    }
                                } else {
                                    Err("LLEN command expects exactly one argument (key)")
                                }
                            }

                            "LRANGE" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(start_bs) = &values[2] {
                                            if let Value::BulkString(end_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let start = String::from_utf8_lossy(start_bs)
                                                    .into_owned()
                                                    .parse::<isize>()
                                                    .map_err(|_| {
                                                        "LRANGE start is not an integer"
                                                    })?;
                                                let end = String::from_utf8_lossy(end_bs)
                                                    .into_owned()
                                                    .parse::<isize>()
                                                    .map_err(|_| "LRANGE end is not an integer")?;
                                                Ok(Command::LRange { key, start, end })
                                            } else {
                                                Err("LRANGE end must be a bulk string")
                                            }
                                        } else {
                                            Err("LRANGE start must be a bulk string")
                                        }
                                    } else {
                                        Err("LRANGE key must be a bulk string")
                                    }
                                } else {
                                    Err("LRANGE command expects exactly three arguments (key, start, end)")
                                }
                            }

                            "LINDEX" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(index_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let index = String::from_utf8_lossy(index_bs)
                                                .into_owned()
                                                .parse::<isize>()
                                                .map_err(|_| "LINDEX index is not an integer")?;
                                            Ok(Command::LIndex { key, index })
                                        } else {
                                            Err("LINDEX index must be a bulk string")
                                        }
                                    } else {
                                        Err("LINDEX key must be a bulk string")
                                    }
                                } else {
                                    Err("LINDEX command expects exactly two arguments (key and index)")
                                }
                            }

                            "LINSERT" => {
                                if values.len() == 5 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(before_after_bs) = &values[2] {
                                            let before_after_str =
                                                String::from_utf8_lossy(before_after_bs)
                                                    .to_uppercase();
                                            let before_after =
                                                match before_after_str.as_str() {
                                                    "BEFORE" => InsertOption::Before,
                                                    "AFTER" => InsertOption::After,
                                                    _ => return Err(
                                                        "LINSERT option must be BEFORE or AFTER",
                                                    ),
                                                };
                                            if let Value::BulkString(pivot_bs) = &values[3] {
                                                if let Value::BulkString(value_bs) = &values[4] {
                                                    let key = String::from_utf8_lossy(key_bs)
                                                        .into_owned();
                                                    let pivot = pivot_bs.clone();
                                                    let value = value_bs.clone();
                                                    Ok(Command::LInsert {
                                                        key,
                                                        before_after,
                                                        pivot,
                                                        value,
                                                    })
                                                } else {
                                                    Err("LINSERT value must be a bulk string")
                                                }
                                            } else {
                                                Err("LINSERT pivot must be a bulk string")
                                            }
                                        } else {
                                            Err("LINSERT BEFORE|AFTER must be a bulk string")
                                        }
                                    } else {
                                        Err("LINSERT key must be a bulk string")
                                    }
                                } else {
                                    Err("LINSERT command expects exactly four arguments (key, BEFORE|AFTER, pivot, value)")
                                }
                            }

                            "LSET" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(index_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let index = String::from_utf8_lossy(index_bs)
                                                    .into_owned()
                                                    .parse::<isize>()
                                                    .map_err(|_| "LSET index is not an integer")?;
                                                let value = value_bs.clone();
                                                Ok(Command::LSet { key, index, value })
                                            } else {
                                                Err("LSET value must be a bulk string")
                                            }
                                        } else {
                                            Err("LSET index must be a bulk string")
                                        }
                                    } else {
                                        Err("LSET key must be a bulk string")
                                    }
                                } else {
                                    Err("LSET command expects exactly three arguments (key, index, value)")
                                }
                            }

                            "LTRIM" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(start_bs) = &values[2] {
                                            if let Value::BulkString(end_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let start = String::from_utf8_lossy(start_bs)
                                                    .into_owned()
                                                    .parse::<isize>()
                                                    .map_err(|_| "LTRIM start is not an integer")?;
                                                let end = String::from_utf8_lossy(end_bs)
                                                    .into_owned()
                                                    .parse::<isize>()
                                                    .map_err(|_| "LTRIM end is not an integer")?;
                                                Ok(Command::LTrim { key, start, end })
                                            } else {
                                                Err("LTRIM end must be a bulk string")
                                            }
                                        } else {
                                            Err("LTRIM start must be a bulk string")
                                        }
                                    } else {
                                        Err("LTRIM key must be a bulk string")
                                    }
                                } else {
                                    Err("LTRIM command expects exactly three arguments (key, start, end)")
                                }
                            }

                            "LREM" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(count_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let count = String::from_utf8_lossy(count_bs)
                                                    .into_owned()
                                                    .parse::<i64>()
                                                    .map_err(|_| "LREM count is not an integer")?;
                                                let value = value_bs.clone();
                                                Ok(Command::LRem { key, count, value })
                                            } else {
                                                Err("LREM value must be a bulk string")
                                            }
                                        } else {
                                            Err("LREM count must be a bulk string")
                                        }
                                    } else {
                                        Err("LREM key must be a bulk string")
                                    }
                                } else {
                                    Err("LREM command expects exactly three arguments (key, count, value)")
                                }
                            }

                            "RPOPLPUSH" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(source_bs) = &values[1] {
                                        if let Value::BulkString(destination_bs) = &values[2] {
                                            let source =
                                                String::from_utf8_lossy(source_bs).into_owned();
                                            let destination =
                                                String::from_utf8_lossy(destination_bs)
                                                    .into_owned();
                                            Ok(Command::RPopLPush {
                                                source,
                                                destination,
                                            })
                                        } else {
                                            Err("RPOPLPUSH destination must be a bulk string")
                                        }
                                    } else {
                                        Err("RPOPLPUSH source must be a bulk string")
                                    }
                                } else {
                                    Err("RPOPLPUSH command expects exactly two arguments (source and destination)")
                                }
                            }

                            "BLPOP" => {
                                if values.len() >= 3 {
                                    let mut keys: Vec<String> = Vec::new();
                                    for value in values.iter().take(values.len() - 1).skip(1) {
                                        if let Value::BulkString(key_bs) = value {
                                            keys.push(String::from_utf8_lossy(key_bs).into_owned());
                                        } else {
                                            return Err("BLPOP keys must be bulk strings");
                                        }
                                    }
                                    if let Value::BulkString(timeout_bs) = &values.last().unwrap() {
                                        let timeout = String::from_utf8_lossy(timeout_bs)
                                            .into_owned()
                                            .parse::<f64>()
                                            .map_err(|_| "BLPOP timeout is not a float")?;
                                        Ok(Command::BLPop { keys, timeout })
                                    } else {
                                        Err("BLPOP timeout must be a bulk string")
                                    }
                                } else {
                                    Err("BLPOP command expects at least one key and a timeout")
                                }
                            }

                            "BRPOP" => {
                                if values.len() >= 3 {
                                    let mut keys: Vec<String> = Vec::new();
                                    for value in values.iter().take(values.len() - 1).skip(1) {
                                        if let Value::BulkString(key_bs) = value {
                                            keys.push(String::from_utf8_lossy(key_bs).into_owned());
                                        } else {
                                            return Err("BRPOP keys must be bulk strings");
                                        }
                                    }
                                    if let Value::BulkString(timeout_bs) = &values.last().unwrap() {
                                        let timeout = String::from_utf8_lossy(timeout_bs)
                                            .into_owned()
                                            .parse::<f64>()
                                            .map_err(|_| "BRPOP timeout is not a float")?;
                                        Ok(Command::BRPop { keys, timeout })
                                    } else {
                                        Err("BRPOP timeout must be a bulk string")
                                    }
                                } else {
                                    Err("BRPOP command expects at least one key and a timeout")
                                }
                            }

                            "JSON.SET" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let path =
                                                    String::from_utf8_lossy(path_bs).into_owned();
                                                let value =
                                                    String::from_utf8_lossy(value_bs).into_owned();
                                                Ok(Command::JsonSet { key, path, value })
                                            } else {
                                                Err("JSON.SET value must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.SET path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.SET key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.SET command expects exactly three arguments (key, path, value)")
                                }
                            }

                            "JSON.GET" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err("JSON.GET path must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonGet { key, path })
                                    } else {
                                        Err("JSON.GET key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.GET command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.DEL" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err("JSON.DEL path must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonDel { key, path })
                                    } else {
                                        Err("JSON.DEL key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.DEL command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.TYPE" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err("JSON.TYPE path must be a bulk string");
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonType { key, path })
                                    } else {
                                        Err("JSON.TYPE key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.TYPE command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.NUMINCRBY" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(increment_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let path =
                                                    String::from_utf8_lossy(path_bs).into_owned();
                                                let increment = String::from_utf8_lossy(
                                                    increment_bs,
                                                )
                                                .into_owned()
                                                .parse::<f64>()
                                                .map_err(|_| {
                                                    "JSON.NUMINCRBY increment is not a float"
                                                })?;
                                                Ok(Command::JsonNumIncrBy {
                                                    key,
                                                    path,
                                                    increment,
                                                })
                                            } else {
                                                Err("JSON.NUMINCRBY increment must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.NUMINCRBY path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.NUMINCRBY key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.NUMINCRBY command expects exactly three arguments (key, path, increment)")
                                }
                            }

                            "JSON.STRAPPEND" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let path =
                                                    String::from_utf8_lossy(path_bs).into_owned();
                                                let value =
                                                    String::from_utf8_lossy(value_bs).into_owned();
                                                Ok(Command::JsonStrAppend { key, path, value })
                                            } else {
                                                Err("JSON.STRAPPEND value must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.STRAPPEND path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.STRAPPEND key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.STRAPPEND command expects exactly three arguments (key, path, value)")
                                }
                            }

                            "JSON.ARRAPPEND" => {
                                if values.len() >= 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let path =
                                                String::from_utf8_lossy(path_bs).into_owned();
                                            let mut values_list: Vec<String> = Vec::new();
                                            for value in values.iter().skip(3) {
                                                if let Value::BulkString(value_bs) = value {
                                                    values_list.push(
                                                        String::from_utf8_lossy(value_bs)
                                                            .into_owned(),
                                                    );
                                                } else {
                                                    return Err("JSON.ARRAPPEND values must be bulk strings");
                                                }
                                            }
                                            Ok(Command::JsonArrAppend {
                                                key,
                                                path,
                                                values: values_list,
                                            })
                                        } else {
                                            Err("JSON.ARRAPPEND path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.ARRAPPEND key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRAPPEND command expects at least two value arguments after key and path")
                                }
                            }

                            "JSON.OBJSET" => {
                                if values.len() == 5 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(key_to_set_bs) = &values[3] {
                                                if let Value::BulkString(value_bs) = &values[4] {
                                                    let key = String::from_utf8_lossy(key_bs)
                                                        .into_owned();
                                                    let path = String::from_utf8_lossy(path_bs)
                                                        .into_owned();
                                                    let key_to_set =
                                                        String::from_utf8_lossy(key_to_set_bs)
                                                            .into_owned();
                                                    let value = String::from_utf8_lossy(value_bs)
                                                        .into_owned();
                                                    Ok(Command::JsonObjSet {
                                                        key,
                                                        path,
                                                        key_to_set,
                                                        value,
                                                    })
                                                } else {
                                                    Err("JSON.OBJSET value must be a bulk string")
                                                }
                                            } else {
                                                Err("JSON.OBJSET object key to set must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.OBJSET path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.OBJSET key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.OBJSET command expects exactly four arguments (key, path, object-key, value)")
                                }
                            }

                            "JSON.OBJKEYS" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err(
                                                    "JSON.OBJKEYS path must be a bulk string",
                                                );
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonObjKeys { key, path })
                                    } else {
                                        Err("JSON.OBJKEYS key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.OBJKEYS command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.OBJLEN" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err(
                                                    "JSON.OBJLEN path must be a bulk string",
                                                );
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonObjLen { key, path })
                                    } else {
                                        Err("JSON.OBJLEN key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.OBJLEN command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.ARRINDEX" => {
                                if values.len() >= 4 && values.len() <= 5 {
                                    // JSON.ARRINDEX key path value [start [stop]]
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let path =
                                                    String::from_utf8_lossy(path_bs).into_owned();
                                                let value =
                                                    String::from_utf8_lossy(value_bs).into_owned();
                                                let range = if values.len() == 5 {
                                                    if let Value::BulkString(range_start_bs) =
                                                        &values[4]
                                                    {
                                                        let start = String::from_utf8_lossy(
                                                            range_start_bs,
                                                        )
                                                        .into_owned()
                                                        .parse::<isize>()
                                                        .map_err(|_| {
                                                            "JSON.ARRINDEX start is not an integer"
                                                        })?;
                                                        Some((start, -1)) // Stop is optional in our command enum, using -1 as placeholder, will adjust in cmd handler if needed
                                                    } else {
                                                        return Err("JSON.ARRINDEX start must be a bulk string");
                                                    }
                                                } else {
                                                    None
                                                };
                                                Ok(Command::JsonArrIndex {
                                                    key,
                                                    path,
                                                    value,
                                                    range,
                                                })
                                            } else {
                                                Err("JSON.ARRINDEX value to find must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.ARRINDEX path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.ARRINDEX key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRINDEX command expects three to four arguments (key, path, value, [start])")
                                }
                            }

                            "JSON.ARRINSERT" => {
                                if values.len() >= 5 {
                                    // JSON.ARRINSERT key path index value [value ...]
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(index_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let path =
                                                    String::from_utf8_lossy(path_bs).into_owned();
                                                let index = String::from_utf8_lossy(index_bs)
                                                    .into_owned()
                                                    .parse::<isize>()
                                                    .map_err(|_| {
                                                        "JSON.ARRINSERT index is not an integer"
                                                    })?;
                                                let mut values_list: Vec<String> = Vec::new();
                                                for value in values.iter().skip(4) {
                                                    if let Value::BulkString(value_bs) = value {
                                                        values_list.push(
                                                            String::from_utf8_lossy(value_bs)
                                                                .into_owned(),
                                                        );
                                                    } else {
                                                        return Err("JSON.ARRINSERT insert values must be bulk strings");
                                                    }
                                                }
                                                Ok(Command::JsonArrInsert {
                                                    key,
                                                    path,
                                                    index,
                                                    values: values_list,
                                                })
                                            } else {
                                                Err("JSON.ARRINSERT index must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.ARRINSERT path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.ARRINSERT key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRINSERT command expects at least three value arguments after key, path and index")
                                }
                            }

                            "JSON.ARRLEN" => {
                                if values.len() >= 2 && values.len() <= 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let path = if values.len() == 3 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                String::from_utf8_lossy(path_bs).into_owned()
                                            } else {
                                                return Err(
                                                    "JSON.ARRLEN path must be a bulk string",
                                                );
                                            }
                                        } else {
                                            ".".to_string() // Default path to root if not provided
                                        };
                                        Ok(Command::JsonArrLen { key, path })
                                    } else {
                                        Err("JSON.ARRLEN key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRLEN command expects one or two arguments (key, [path])")
                                }
                            }

                            "JSON.ARRPOP" => {
                                if values.len() >= 2 && values.len() <= 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut path = "$".to_string();
                                        let mut index = None;

                                        if values.len() == 3 {
                                            if let Value::BulkString(arg_bs) = &values[2] {
                                                let arg =
                                                    String::from_utf8_lossy(arg_bs).into_owned();
                                                if let Ok(parsed_index) = arg.parse::<isize>() {
                                                    index = Some(parsed_index);
                                                } else {
                                                    path = arg;
                                                }
                                            } else {
                                                return Err(
                                                    "JSON.ARRPOP path/index must be a bulk string",
                                                );
                                            }
                                        } else if values.len() == 4 {
                                            if let Value::BulkString(path_bs) = &values[2] {
                                                path =
                                                    String::from_utf8_lossy(path_bs).into_owned();
                                            } else {
                                                return Err(
                                                    "JSON.ARRPOP path must be a bulk string",
                                                );
                                            }
                                            if let Value::BulkString(index_bs) = &values[3] {
                                                index = Some(
                                                    String::from_utf8_lossy(index_bs)
                                                        .parse::<isize>()
                                                        .map_err(|_| {
                                                            "JSON.ARRPOP index is not an integer"
                                                        })?,
                                                );
                                            } else {
                                                return Err(
                                                    "JSON.ARRPOP index must be a bulk string",
                                                );
                                            }
                                        }

                                        Ok(Command::JsonArrPop { key, path, index })
                                    } else {
                                        Err("JSON.ARRPOP key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRPOP command expects key [path [index]]")
                                }
                            }

                            "JSON.ARRTRIM" => {
                                if values.len() == 5 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(path_bs) = &values[2] {
                                            if let Value::BulkString(start_bs) = &values[3] {
                                                if let Value::BulkString(stop_bs) = &values[4] {
                                                    let key = String::from_utf8_lossy(key_bs)
                                                        .into_owned();
                                                    let path = String::from_utf8_lossy(path_bs)
                                                        .into_owned();
                                                    let start = String::from_utf8_lossy(start_bs)
                                                        .into_owned()
                                                        .parse::<isize>()
                                                        .map_err(|_| {
                                                            "JSON.ARRTRIM start is not an integer"
                                                        })?;
                                                    let stop = String::from_utf8_lossy(stop_bs)
                                                        .into_owned()
                                                        .parse::<isize>()
                                                        .map_err(|_| {
                                                            "JSON.ARRTRIM stop is not an integer"
                                                        })?;

                                                    Ok(Command::JsonArrTrim {
                                                        key,
                                                        path,
                                                        start,
                                                        stop,
                                                    })
                                                } else {
                                                    Err("JSON.ARRTRIM stop must be a bulk string")
                                                }
                                            } else {
                                                Err("JSON.ARRTRIM start must be a bulk string")
                                            }
                                        } else {
                                            Err("JSON.ARRTRIM path must be a bulk string")
                                        }
                                    } else {
                                        Err("JSON.ARRTRIM key must be a bulk string")
                                    }
                                } else {
                                    Err("JSON.ARRTRIM command expects exactly four arguments (key, path, start, stop)")
                                }
                            }

                            // Graph Commands
                            "GRAPH.CREATE_NODE" => {
                                if values.len() >= 3 && (values.len() - 3).is_multiple_of(2) {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(node_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let node_id =
                                                String::from_utf8_lossy(node_id_bs).into_owned();
                                            let mut properties = Vec::new();
                                            let mut i = 3;
                                            while i < values.len() {
                                                if let (
                                                    Value::BulkString(prop_key_bs),
                                                    Value::BulkString(prop_value_bs),
                                                ) = (&values[i], &values[i + 1])
                                                {
                                                    properties.push((
                                                        String::from_utf8_lossy(prop_key_bs)
                                                            .into_owned(),
                                                        String::from_utf8_lossy(prop_value_bs)
                                                            .into_owned(),
                                                    ));
                                                    i += 2;
                                                } else {
                                                    return Err("GRAPH.CREATE_NODE properties must be key value pairs as bulk strings");
                                                }
                                            }
                                            Ok(Command::GraphCreateNode {
                                                key,
                                                node_id,
                                                properties,
                                            })
                                        } else {
                                            Err("GRAPH.CREATE_NODE node_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.CREATE_NODE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.CREATE_NODE expects at least key and node_id, followed by property key-value pairs")
                                }
                            }

                            "GRAPH.GET_NODE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(node_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let node_id =
                                                String::from_utf8_lossy(node_id_bs).into_owned();
                                            Ok(Command::GraphGetNode { key, node_id })
                                        } else {
                                            Err("GRAPH.GET_NODE node_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.GET_NODE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.GET_NODE command expects exactly two arguments (key and node_id)")
                                }
                            }

                            "GRAPH.DELETE_NODE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(node_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let node_id =
                                                String::from_utf8_lossy(node_id_bs).into_owned();
                                            Ok(Command::GraphDeleteNode { key, node_id })
                                        } else {
                                            Err("GRAPH.DELETE_NODE node_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.DELETE_NODE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.DELETE_NODE command expects exactly two arguments (key and node_id)")
                                }
                            }

                            "GRAPH.CREATE_EDGE" => {
                                if values.len() >= 6 && (values.len() - 6).is_multiple_of(2) {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(edge_id_bs) = &values[2] {
                                            if let Value::BulkString(source_id_bs) = &values[3] {
                                                if let Value::BulkString(target_id_bs) = &values[4]
                                                {
                                                    if let Value::BulkString(relation_type_bs) =
                                                        &values[5]
                                                    {
                                                        let key = String::from_utf8_lossy(key_bs)
                                                            .into_owned();
                                                        let edge_id =
                                                            String::from_utf8_lossy(edge_id_bs)
                                                                .into_owned();
                                                        let source_node_id =
                                                            String::from_utf8_lossy(source_id_bs)
                                                                .into_owned();
                                                        let target_node_id =
                                                            String::from_utf8_lossy(target_id_bs)
                                                                .into_owned();
                                                        let relation_type =
                                                            String::from_utf8_lossy(
                                                                relation_type_bs,
                                                            )
                                                            .into_owned();
                                                        let mut properties = Vec::new();
                                                        let mut i = 6;
                                                        while i < values.len() {
                                                            if let (
                                                                Value::BulkString(prop_key_bs),
                                                                Value::BulkString(prop_value_bs),
                                                            ) = (&values[i], &values[i + 1])
                                                            {
                                                                properties.push((
                                                                    String::from_utf8_lossy(
                                                                        prop_key_bs,
                                                                    )
                                                                    .into_owned(),
                                                                    String::from_utf8_lossy(
                                                                        prop_value_bs,
                                                                    )
                                                                    .into_owned(),
                                                                ));
                                                                i += 2;
                                                            } else {
                                                                return Err("GRAPH.CREATE_EDGE properties must be key value pairs as bulk strings");
                                                            }
                                                        }
                                                        Ok(Command::GraphCreateEdge {
                                                            key,
                                                            edge_id,
                                                            source_node_id,
                                                            target_node_id,
                                                            relation_type,
                                                            properties,
                                                        })
                                                    } else {
                                                        Err("GRAPH.CREATE_EDGE relation_type must be a bulk string")
                                                    }
                                                } else {
                                                    Err("GRAPH.CREATE_EDGE target_node_id must be a bulk string")
                                                }
                                            } else {
                                                Err("GRAPH.CREATE_EDGE source_node_id must be a bulk string")
                                            }
                                        } else {
                                            Err("GRAPH.CREATE_EDGE edge_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.CREATE_EDGE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.CREATE_EDGE expects at least key, edge_id, source_node_id, target_node_id, relation_type, followed by property key-value pairs")
                                }
                            }

                            "GRAPH.GET_EDGE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(edge_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let edge_id =
                                                String::from_utf8_lossy(edge_id_bs).into_owned();
                                            Ok(Command::GraphGetEdge { key, edge_id })
                                        } else {
                                            Err("GRAPH.GET_EDGE edge_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.GET_EDGE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.GET_EDGE command expects exactly two arguments (key and edge_id)")
                                }
                            }

                            "GRAPH.DELETE_EDGE" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(edge_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let edge_id =
                                                String::from_utf8_lossy(edge_id_bs).into_owned();
                                            Ok(Command::GraphDeleteEdge { key, edge_id })
                                        } else {
                                            Err("GRAPH.DELETE_EDGE edge_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.DELETE_EDGE key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.DELETE_EDGE command expects exactly two arguments (key and edge_id)")
                                }
                            }

                            "GRAPH.GET_NODE_PROPERTIES" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(node_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let node_id =
                                                String::from_utf8_lossy(node_id_bs).into_owned();
                                            Ok(Command::GraphGetNodeProperties { key, node_id })
                                        } else {
                                            Err("GRAPH.GET_NODE_PROPERTIES node_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.GET_NODE_PROPERTIES key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.GET_NODE_PROPERTIES expects exactly two arguments (key and node_id)")
                                }
                            }
                            "GRAPH.SET_NODE_PROPERTY" => {
                                if values.len() == 5 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(node_id_bs) = &values[2] {
                                            if let Value::BulkString(property_key_bs) = &values[3] {
                                                if let Value::BulkString(property_value_bs) =
                                                    &values[4]
                                                {
                                                    let key = String::from_utf8_lossy(key_bs)
                                                        .into_owned();
                                                    let node_id =
                                                        String::from_utf8_lossy(node_id_bs)
                                                            .into_owned();
                                                    let property_key =
                                                        String::from_utf8_lossy(property_key_bs)
                                                            .into_owned();
                                                    let property_value =
                                                        String::from_utf8_lossy(property_value_bs)
                                                            .into_owned();
                                                    Ok(Command::GraphSetNodeProperty {
                                                        key,
                                                        node_id,
                                                        property_key,
                                                        property_value,
                                                    })
                                                } else {
                                                    Err("GRAPH.SET_NODE_PROPERTY property_value must be a bulk string")
                                                }
                                            } else {
                                                Err("GRAPH.SET_NODE_PROPERTY property_key must be a bulk string")
                                            }
                                        } else {
                                            Err("GRAPH.SET_NODE_PROPERTY node_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.SET_NODE_PROPERTY key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.SET_NODE_PROPERTY expects exactly four arguments (key, node_id, property_key, property_value)")
                                }
                            }
                            "GRAPH.DELETE_NODE_PROPERTY" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(node_id_bs) = &values[2] {
                                            if let Value::BulkString(property_key_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let node_id = String::from_utf8_lossy(node_id_bs)
                                                    .into_owned();
                                                let property_key =
                                                    String::from_utf8_lossy(property_key_bs)
                                                        .into_owned();
                                                Ok(Command::GraphDeleteNodeProperty {
                                                    key,
                                                    node_id,
                                                    property_key,
                                                })
                                            } else {
                                                Err("GRAPH.DELETE_NODE_PROPERTY property_key must be a bulk string")
                                            }
                                        } else {
                                            Err("GRAPH.DELETE_NODE_PROPERTY node_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.DELETE_NODE_PROPERTY key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.DELETE_NODE_PROPERTY expects exactly three arguments (key, node_id, property_key)")
                                }
                            }
                            "GRAPH.GET_EDGE_PROPERTIES" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(edge_id_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let edge_id =
                                                String::from_utf8_lossy(edge_id_bs).into_owned();
                                            Ok(Command::GraphGetEdgeProperties { key, edge_id })
                                        } else {
                                            Err("GRAPH.GET_EDGE_PROPERTIES edge_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.GET_EDGE_PROPERTIES key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.GET_EDGE_PROPERTIES expects exactly two arguments (key and edge_id)")
                                }
                            }
                            "GRAPH.SET_EDGE_PROPERTY" => {
                                if values.len() == 5 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(edge_id_bs) = &values[2] {
                                            if let Value::BulkString(property_key_bs) = &values[3] {
                                                if let Value::BulkString(property_value_bs) =
                                                    &values[4]
                                                {
                                                    let key = String::from_utf8_lossy(key_bs)
                                                        .into_owned();
                                                    let edge_id =
                                                        String::from_utf8_lossy(edge_id_bs)
                                                            .into_owned();
                                                    let property_key =
                                                        String::from_utf8_lossy(property_key_bs)
                                                            .into_owned();
                                                    let property_value =
                                                        String::from_utf8_lossy(property_value_bs)
                                                            .into_owned();
                                                    Ok(Command::GraphSetEdgeProperty {
                                                        key,
                                                        edge_id,
                                                        property_key,
                                                        property_value,
                                                    })
                                                } else {
                                                    Err("GRAPH.SET_EDGE_PROPERTY property_value must be a bulk string")
                                                }
                                            } else {
                                                Err("GRAPH.SET_EDGE_PROPERTY property_key must be a bulk string")
                                            }
                                        } else {
                                            Err("GRAPH.SET_EDGE_PROPERTY edge_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.SET_EDGE_PROPERTY key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.SET_EDGE_PROPERTY expects exactly four arguments (key, edge_id, property_key, property_value)")
                                }
                            }
                            "GRAPH.DELETE_EDGE_PROPERTY" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(edge_id_bs) = &values[2] {
                                            if let Value::BulkString(property_key_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let edge_id = String::from_utf8_lossy(edge_id_bs)
                                                    .into_owned();
                                                let property_key =
                                                    String::from_utf8_lossy(property_key_bs)
                                                        .into_owned();
                                                Ok(Command::GraphDeleteEdgeProperty {
                                                    key,
                                                    edge_id,
                                                    property_key,
                                                })
                                            } else {
                                                Err("GRAPH.DELETE_EDGE_PROPERTY property_key must be a bulk string")
                                            }
                                        } else {
                                            Err("GRAPH.DELETE_EDGE_PROPERTY edge_id must be a bulk string")
                                        }
                                    } else {
                                        Err("GRAPH.DELETE_EDGE_PROPERTY key must be a bulk string")
                                    }
                                } else {
                                    Err("GRAPH.DELETE_EDGE_PROPERTY expects exactly three arguments (key, edge_id, property_key)")
                                }
                            }

                            // Bitmap & HyperLogLog commands
                            "SETBIT" => {
                                if values.len() == 4 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(offset_bs) = &values[2] {
                                            if let Value::BulkString(value_bs) = &values[3] {
                                                let key =
                                                    String::from_utf8_lossy(key_bs).into_owned();
                                                let offset = String::from_utf8_lossy(offset_bs)
                                                    .parse::<u64>()
                                                    .map_err(|_| {
                                                        "bit offset is not an integer or out of range"
                                                    })?;
                                                let value = String::from_utf8_lossy(value_bs)
                                                    .parse::<u8>()
                                                    .map_err(|_| {
                                                        "ERR bit is not an integer or out of range"
                                                    })?;
                                                if value != 0 && value != 1 {
                                                    return Err(
                                                        "ERR bit is not an integer or out of range",
                                                    );
                                                }
                                                Ok(Command::SetBit { key, offset, value })
                                            } else {
                                                Err("SETBIT value must be a bulk string")
                                            }
                                        } else {
                                            Err("SETBIT offset must be a bulk string")
                                        }
                                    } else {
                                        Err("SETBIT key must be a bulk string")
                                    }
                                } else {
                                    Err("SETBIT command expects exactly three arguments (key, offset, value)")
                                }
                            }
                            "GETBIT" => {
                                if values.len() == 3 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        if let Value::BulkString(offset_bs) = &values[2] {
                                            let key = String::from_utf8_lossy(key_bs).into_owned();
                                            let offset = String::from_utf8_lossy(offset_bs)
                                                .parse::<u64>()
                                                .map_err(|_| {
                                                    "bit offset is not an integer or out of range"
                                                })?;
                                            Ok(Command::GetBit { key, offset })
                                        } else {
                                            Err("GETBIT offset must be a bulk string")
                                        }
                                    } else {
                                        Err("GETBIT key must be a bulk string")
                                    }
                                } else {
                                    Err("GETBIT command expects exactly two arguments (key, offset)")
                                }
                            }
                            "BITCOUNT" => {
                                if values.len() == 2 || values.len() == 4 || values.len() == 5 {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let mut range = None;
                                        let mut bit_unit = false;
                                        if values.len() >= 4 {
                                            let start = if let Value::BulkString(start_bs) =
                                                &values[2]
                                            {
                                                String::from_utf8_lossy(start_bs)
                                                    .parse::<i64>()
                                                    .map_err(|_| {
                                                        "value is not an integer or out of range"
                                                    })?
                                            } else {
                                                return Err("BITCOUNT start must be a bulk string");
                                            };
                                            let end = if let Value::BulkString(end_bs) = &values[3]
                                            {
                                                String::from_utf8_lossy(end_bs)
                                                    .parse::<i64>()
                                                    .map_err(|_| {
                                                        "value is not an integer or out of range"
                                                    })?
                                            } else {
                                                return Err("BITCOUNT end must be a bulk string");
                                            };
                                            range = Some((start, end));
                                        }
                                        if values.len() == 5 {
                                            if let Value::BulkString(unit_bs) = &values[4] {
                                                match String::from_utf8_lossy(unit_bs)
                                                    .to_uppercase()
                                                    .as_str()
                                                {
                                                    "BIT" => bit_unit = true,
                                                    "BYTE" => bit_unit = false,
                                                    _ => return Err("ERR syntax error"),
                                                }
                                            } else {
                                                return Err("BITCOUNT unit must be a bulk string");
                                            }
                                        }
                                        Ok(Command::BitCount {
                                            key,
                                            range,
                                            bit_unit,
                                        })
                                    } else {
                                        Err("BITCOUNT key must be a bulk string")
                                    }
                                } else {
                                    Err("BITCOUNT command expects key [start end [BYTE|BIT]]")
                                }
                            }
                            "BITPOS" => {
                                if (3..=6).contains(&values.len()) {
                                    if let Value::BulkString(key_bs) = &values[1] {
                                        let key = String::from_utf8_lossy(key_bs).into_owned();
                                        let bit = if let Value::BulkString(bit_bs) = &values[2] {
                                            String::from_utf8_lossy(bit_bs).parse::<u8>().map_err(
                                                |_| "ERR bit is not an integer or out of range",
                                            )?
                                        } else {
                                            return Err("BITPOS bit must be a bulk string");
                                        };
                                        if bit != 0 && bit != 1 {
                                            return Err(
                                                "ERR bit is not an integer or out of range",
                                            );
                                        }
                                        let mut start = None;
                                        let mut end = None;
                                        let mut bit_unit = false;
                                        if values.len() >= 4 {
                                            if let Value::BulkString(start_bs) = &values[3] {
                                                start = Some(
                                                    String::from_utf8_lossy(start_bs)
                                                        .parse::<i64>()
                                                        .map_err(|_| {
                                                            "value is not an integer or out of range"
                                                        })?,
                                                );
                                            } else {
                                                return Err("BITPOS start must be a bulk string");
                                            }
                                        }
                                        if values.len() >= 5 {
                                            if let Value::BulkString(end_bs) = &values[4] {
                                                end = Some(
                                                    String::from_utf8_lossy(end_bs)
                                                        .parse::<i64>()
                                                        .map_err(|_| {
                                                            "value is not an integer or out of range"
                                                        })?,
                                                );
                                            } else {
                                                return Err("BITPOS end must be a bulk string");
                                            }
                                        }
                                        if values.len() == 6 {
                                            if let Value::BulkString(unit_bs) = &values[5] {
                                                match String::from_utf8_lossy(unit_bs)
                                                    .to_uppercase()
                                                    .as_str()
                                                {
                                                    "BIT" => bit_unit = true,
                                                    "BYTE" => bit_unit = false,
                                                    _ => return Err("ERR syntax error"),
                                                }
                                            } else {
                                                return Err("BITPOS unit must be a bulk string");
                                            }
                                        }
                                        Ok(Command::BitPos {
                                            key,
                                            bit,
                                            start,
                                            end,
                                            bit_unit,
                                        })
                                    } else {
                                        Err("BITPOS key must be a bulk string")
                                    }
                                } else {
                                    Err("BITPOS command expects key bit [start [end [BYTE|BIT]]]")
                                }
                            }
                            "BITOP" => {
                                if values.len() >= 4 {
                                    let op = if let Value::BulkString(op_bs) = &values[1] {
                                        String::from_utf8_lossy(op_bs).to_uppercase()
                                    } else {
                                        return Err("BITOP operation must be a bulk string");
                                    };
                                    if op != "AND" && op != "OR" && op != "XOR" && op != "NOT" {
                                        return Err("ERR syntax error");
                                    }
                                    let dest = if let Value::BulkString(dest_bs) = &values[2] {
                                        String::from_utf8_lossy(dest_bs).into_owned()
                                    } else {
                                        return Err("BITOP destkey must be a bulk string");
                                    };
                                    let mut keys = Vec::with_capacity(values.len() - 3);
                                    for v in &values[3..] {
                                        if let Value::BulkString(k_bs) = v {
                                            keys.push(String::from_utf8_lossy(k_bs).into_owned());
                                        } else {
                                            return Err("BITOP keys must be bulk strings");
                                        }
                                    }
                                    if op == "NOT" && keys.len() != 1 {
                                        return Err(
                                            "ERR BITOP NOT must be called with a single source key.",
                                        );
                                    }
                                    Ok(Command::BitOp { op, dest, keys })
                                } else {
                                    Err("BITOP command expects at least three arguments (operation, destkey, key [key ...])")
                                }
                            }
                            "PFADD" => {
                                if values.len() >= 2 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("PFADD key must be a bulk string");
                                    };
                                    let mut elements = Vec::with_capacity(values.len() - 2);
                                    for v in &values[2..] {
                                        if let Value::BulkString(e_bs) = v {
                                            elements.push(e_bs.clone());
                                        } else {
                                            return Err("PFADD elements must be bulk strings");
                                        }
                                    }
                                    Ok(Command::PfAdd { key, elements })
                                } else {
                                    Err("PFADD command expects at least one argument (key [element ...])")
                                }
                            }
                            "PFCOUNT" => {
                                if values.len() >= 2 {
                                    let mut keys = Vec::with_capacity(values.len() - 1);
                                    for v in &values[1..] {
                                        if let Value::BulkString(k_bs) = v {
                                            keys.push(String::from_utf8_lossy(k_bs).into_owned());
                                        } else {
                                            return Err("PFCOUNT keys must be bulk strings");
                                        }
                                    }
                                    Ok(Command::PfCount { keys })
                                } else {
                                    Err("PFCOUNT command expects at least one argument (key [key ...])")
                                }
                            }
                            "PFMERGE" => {
                                if values.len() >= 2 {
                                    let dest = if let Value::BulkString(dest_bs) = &values[1] {
                                        String::from_utf8_lossy(dest_bs).into_owned()
                                    } else {
                                        return Err("PFMERGE destkey must be a bulk string");
                                    };
                                    let mut sources = Vec::with_capacity(values.len() - 2);
                                    for v in &values[2..] {
                                        if let Value::BulkString(s_bs) = v {
                                            sources
                                                .push(String::from_utf8_lossy(s_bs).into_owned());
                                        } else {
                                            return Err("PFMERGE sources must be bulk strings");
                                        }
                                    }
                                    Ok(Command::PfMerge { dest, sources })
                                } else {
                                    Err("PFMERGE command expects at least one argument (destkey [sourcekey ...])")
                                }
                            }

                            "GEOADD" => {
                                if values.len() < 5 {
                                    return Err("GEOADD command expects key [NX|XX] [CH] longitude latitude member [longitude latitude member ...]");
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("GEOADD key must be a bulk string");
                                };

                                let mut nx = false;
                                let mut xx = false;
                                let mut ch = false;

                                // Scan optional flags after the key until a token parses as a
                                // float (the start of the first lon/lat/member triple).
                                let mut i = 2;
                                while i < values.len() {
                                    let bs = if let Value::BulkString(bs) = &values[i] {
                                        bs
                                    } else {
                                        return Err("GEOADD arguments must be bulk strings");
                                    };
                                    // A token that parses as a float ends the flag section.
                                    if String::from_utf8_lossy(bs).parse::<f64>().is_ok() {
                                        break;
                                    }
                                    match String::from_utf8_lossy(bs).to_uppercase().as_str() {
                                        "NX" => nx = true,
                                        "XX" => xx = true,
                                        "CH" => ch = true,
                                        _ => return Err("GEOADD longitude is not a valid float"),
                                    }
                                    i += 1;
                                }

                                // Remaining tokens are longitude latitude member triples.
                                if i >= values.len() {
                                    return Err(
                                        "GEOADD command expects at least one longitude latitude member triple",
                                    );
                                }
                                if !(values.len() - i).is_multiple_of(3) {
                                    return Err(
                                        "GEOADD command expects longitude latitude member triples (count multiple of 3)",
                                    );
                                }

                                let mut items: Vec<(f64, f64, Vec<u8>)> = Vec::new();
                                while i < values.len() {
                                    let lon = if let Value::BulkString(lon_bs) = &values[i] {
                                        String::from_utf8_lossy(lon_bs)
                                            .parse::<f64>()
                                            .map_err(|_| "GEOADD longitude is not a valid float")?
                                    } else {
                                        return Err("GEOADD longitude must be a bulk string");
                                    };
                                    let lat = if let Value::BulkString(lat_bs) = &values[i + 1] {
                                        String::from_utf8_lossy(lat_bs)
                                            .parse::<f64>()
                                            .map_err(|_| "GEOADD latitude is not a valid float")?
                                    } else {
                                        return Err("GEOADD latitude must be a bulk string");
                                    };
                                    let member =
                                        if let Value::BulkString(member_bs) = &values[i + 2] {
                                            member_bs.clone()
                                        } else {
                                            return Err("GEOADD member must be a bulk string");
                                        };
                                    items.push((lon, lat, member));
                                    i += 3;
                                }

                                Ok(Command::GeoAdd {
                                    key,
                                    nx,
                                    xx,
                                    ch,
                                    items,
                                })
                            }
                            "GEOPOS" => {
                                if values.len() >= 2 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("GEOPOS key must be a bulk string");
                                    };
                                    let mut members = Vec::with_capacity(values.len() - 2);
                                    for v in &values[2..] {
                                        if let Value::BulkString(m_bs) = v {
                                            members.push(m_bs.clone());
                                        } else {
                                            return Err("GEOPOS members must be bulk strings");
                                        }
                                    }
                                    Ok(Command::GeoPos { key, members })
                                } else {
                                    Err("GEOPOS command expects at least one argument (key [member ...])")
                                }
                            }
                            "GEODIST" => {
                                if values.len() == 4 || values.len() == 5 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("GEODIST key must be a bulk string");
                                    };
                                    let member1 = if let Value::BulkString(m1_bs) = &values[2] {
                                        m1_bs.clone()
                                    } else {
                                        return Err("GEODIST member1 must be a bulk string");
                                    };
                                    let member2 = if let Value::BulkString(m2_bs) = &values[3] {
                                        m2_bs.clone()
                                    } else {
                                        return Err("GEODIST member2 must be a bulk string");
                                    };
                                    let unit = if values.len() == 5 {
                                        if let Value::BulkString(u_bs) = &values[4] {
                                            String::from_utf8_lossy(u_bs).to_lowercase()
                                        } else {
                                            return Err("GEODIST unit must be a bulk string");
                                        }
                                    } else {
                                        "m".to_string()
                                    };
                                    match unit.as_str() {
                                        "m" | "km" | "mi" | "ft" => {}
                                        _ => {
                                            return Err("ERR unsupported unit provided. please use M, KM, FT, MT");
                                        }
                                    }
                                    Ok(Command::GeoDist {
                                        key,
                                        member1,
                                        member2,
                                        unit,
                                    })
                                } else {
                                    Err("GEODIST command expects key member1 member2 [unit]")
                                }
                            }
                            "GEOHASH" => {
                                if values.len() >= 2 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("GEOHASH key must be a bulk string");
                                    };
                                    let mut members = Vec::with_capacity(values.len() - 2);
                                    for v in &values[2..] {
                                        if let Value::BulkString(m_bs) = v {
                                            members.push(m_bs.clone());
                                        } else {
                                            return Err("GEOHASH members must be bulk strings");
                                        }
                                    }
                                    Ok(Command::GeoHash { key, members })
                                } else {
                                    Err("GEOHASH command expects at least one argument (key [member ...])")
                                }
                            }
                            "GEOSEARCH" => {
                                if values.len() < 2 {
                                    return Err("GEOSEARCH command expects key <FROMMEMBER member | FROMLONLAT lon lat> BYRADIUS radius unit [ASC|DESC] [COUNT n] [WITHCOORD] [WITHDIST] [WITHHASH]");
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("GEOSEARCH key must be a bulk string");
                                };

                                let mut from_member: Option<Vec<u8>> = None;
                                let mut from_lonlat: Option<(f64, f64)> = None;
                                let mut radius: Option<f64> = None;
                                let mut unit: Option<String> = None;
                                let mut with_coord = false;
                                let mut with_dist = false;
                                let mut with_hash = false;
                                let mut count: Option<usize> = None;
                                let mut asc: Option<bool> = None;

                                let mut i = 2;
                                while i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err("GEOSEARCH option token must be a bulk string");
                                    };
                                    match token.as_str() {
                                        "FROMMEMBER" => {
                                            if i + 1 >= values.len() {
                                                return Err(
                                                    "GEOSEARCH FROMMEMBER requires a member",
                                                );
                                            }
                                            let member = if let Value::BulkString(m_bs) =
                                                &values[i + 1]
                                            {
                                                m_bs.clone()
                                            } else {
                                                return Err("GEOSEARCH FROMMEMBER member must be a bulk string");
                                            };
                                            from_member = Some(member);
                                            i += 2;
                                        }
                                        "FROMLONLAT" => {
                                            if i + 2 >= values.len() {
                                                return Err("GEOSEARCH FROMLONLAT requires longitude and latitude");
                                            }
                                            let lon = if let Value::BulkString(lon_bs) =
                                                &values[i + 1]
                                            {
                                                String::from_utf8_lossy(lon_bs)
                                                        .parse::<f64>()
                                                        .map_err(|_| "GEOSEARCH FROMLONLAT longitude is not a valid float")?
                                            } else {
                                                return Err("GEOSEARCH FROMLONLAT longitude must be a bulk string");
                                            };
                                            let lat = if let Value::BulkString(lat_bs) =
                                                &values[i + 2]
                                            {
                                                String::from_utf8_lossy(lat_bs)
                                                        .parse::<f64>()
                                                        .map_err(|_| "GEOSEARCH FROMLONLAT latitude is not a valid float")?
                                            } else {
                                                return Err("GEOSEARCH FROMLONLAT latitude must be a bulk string");
                                            };
                                            from_lonlat = Some((lon, lat));
                                            i += 3;
                                        }
                                        "BYRADIUS" => {
                                            if i + 2 >= values.len() {
                                                return Err(
                                                    "GEOSEARCH BYRADIUS requires radius and unit",
                                                );
                                            }
                                            let r = if let Value::BulkString(r_bs) = &values[i + 1]
                                            {
                                                String::from_utf8_lossy(r_bs)
                                                        .parse::<f64>()
                                                        .map_err(|_| "GEOSEARCH BYRADIUS radius is not a valid float")?
                                            } else {
                                                return Err("GEOSEARCH BYRADIUS radius must be a bulk string");
                                            };
                                            let u = if let Value::BulkString(u_bs) = &values[i + 2]
                                            {
                                                String::from_utf8_lossy(u_bs).to_lowercase()
                                            } else {
                                                return Err(
                                                    "GEOSEARCH BYRADIUS unit must be a bulk string",
                                                );
                                            };
                                            match u.as_str() {
                                                "m" | "km" | "mi" | "ft" => {}
                                                _ => {
                                                    return Err("ERR unsupported unit provided. please use M, KM, FT, MT");
                                                }
                                            }
                                            radius = Some(r);
                                            unit = Some(u);
                                            i += 3;
                                        }
                                        "BYBOX" => {
                                            return Err("ERR BYBOX is not supported yet");
                                        }
                                        "ASC" => {
                                            asc = Some(true);
                                            i += 1;
                                        }
                                        "DESC" => {
                                            asc = Some(false);
                                            i += 1;
                                        }
                                        "COUNT" => {
                                            if i + 1 >= values.len() {
                                                return Err("GEOSEARCH COUNT requires a count");
                                            }
                                            let c = if let Value::BulkString(c_bs) = &values[i + 1]
                                            {
                                                String::from_utf8_lossy(c_bs)
                                                    .parse::<usize>()
                                                    .map_err(|_| {
                                                        "GEOSEARCH COUNT is not a valid integer"
                                                    })?
                                            } else {
                                                return Err(
                                                    "GEOSEARCH COUNT must be a bulk string",
                                                );
                                            };
                                            count = Some(c);
                                            i += 2;
                                        }
                                        "WITHCOORD" => {
                                            with_coord = true;
                                            i += 1;
                                        }
                                        "WITHDIST" => {
                                            with_dist = true;
                                            i += 1;
                                        }
                                        "WITHHASH" => {
                                            with_hash = true;
                                            i += 1;
                                        }
                                        _ => {
                                            return Err(
                                                "GEOSEARCH syntax error: unsupported option",
                                            );
                                        }
                                    }
                                }

                                if from_member.is_some() && from_lonlat.is_some() {
                                    return Err(
                                        "GEOSEARCH cannot specify both FROMMEMBER and FROMLONLAT",
                                    );
                                }
                                if from_member.is_none() && from_lonlat.is_none() {
                                    return Err("GEOSEARCH requires exactly one of FROMMEMBER or FROMLONLAT");
                                }

                                let radius = match radius {
                                    Some(r) => r,
                                    None => {
                                        return Err("GEOSEARCH requires BYRADIUS radius unit");
                                    }
                                };
                                let unit = match unit {
                                    Some(u) => u,
                                    None => {
                                        return Err("GEOSEARCH requires BYRADIUS radius unit");
                                    }
                                };

                                Ok(Command::GeoSearch {
                                    key,
                                    from_member,
                                    from_lonlat,
                                    radius,
                                    unit,
                                    with_coord,
                                    with_dist,
                                    with_hash,
                                    count,
                                    asc,
                                })
                            }

                            // Stream commands
                            "XADD" => {
                                // XADD key [NOMKSTREAM] [MAXLEN [=|~] count] <id|*> field value [field value ...]
                                if values.len() < 2 {
                                    return Err(
                                        "XADD command expects key [NOMKSTREAM] [MAXLEN [=|~] count] <id|*> field value [field value ...]",
                                    );
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("XADD key must be a bulk string");
                                };

                                let mut nomkstream = false;
                                let mut maxlen: Option<usize> = None;

                                let mut i = 2;

                                // Optional NOMKSTREAM flag.
                                if i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err("XADD arguments must be bulk strings");
                                    };
                                    if token == "NOMKSTREAM" {
                                        nomkstream = true;
                                        i += 1;
                                    }
                                }

                                // Optional MAXLEN [=|~] count.
                                if i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err("XADD arguments must be bulk strings");
                                    };
                                    if token == "MAXLEN" {
                                        i += 1;
                                        // Optional approximate/exact marker (= or ~); ignore which.
                                        if i < values.len() {
                                            let marker = if let Value::BulkString(bs) = &values[i] {
                                                String::from_utf8_lossy(bs).into_owned()
                                            } else {
                                                return Err("XADD arguments must be bulk strings");
                                            };
                                            if marker == "=" || marker == "~" {
                                                i += 1;
                                            }
                                        }
                                        if i >= values.len() {
                                            return Err("XADD MAXLEN requires a count");
                                        }
                                        let count = if let Value::BulkString(c_bs) = &values[i] {
                                            String::from_utf8_lossy(c_bs)
                                                .parse::<usize>()
                                                .map_err(|_| "XADD MAXLEN count is not a valid integer")?
                                        } else {
                                            return Err("XADD MAXLEN count must be a bulk string");
                                        };
                                        maxlen = Some(count);
                                        i += 1;
                                    }
                                }

                                // The next token is the id (`*` or an explicit id), stored verbatim.
                                if i >= values.len() {
                                    return Err("XADD command expects an id (`*` or explicit id)");
                                }
                                let id = if let Value::BulkString(id_bs) = &values[i] {
                                    String::from_utf8_lossy(id_bs).into_owned()
                                } else {
                                    return Err("XADD id must be a bulk string");
                                };
                                i += 1;

                                // Remaining tokens are field/value pairs (non-empty, even count).
                                let remaining = values.len() - i;
                                if remaining == 0 {
                                    return Err(
                                        "XADD command expects at least one field value pair",
                                    );
                                }
                                if !remaining.is_multiple_of(2) {
                                    return Err(
                                        "XADD command expects field value pairs (even count)",
                                    );
                                }
                                let mut fields: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(remaining / 2);
                                while i < values.len() {
                                    let field = if let Value::BulkString(f_bs) = &values[i] {
                                        f_bs.clone()
                                    } else {
                                        return Err("XADD field must be a bulk string");
                                    };
                                    let value = if let Value::BulkString(v_bs) = &values[i + 1] {
                                        v_bs.clone()
                                    } else {
                                        return Err("XADD value must be a bulk string");
                                    };
                                    fields.push((field, value));
                                    i += 2;
                                }

                                Ok(Command::XAdd {
                                    key,
                                    id,
                                    nomkstream,
                                    maxlen,
                                    fields,
                                })
                            }
                            "XLEN" => {
                                if values.len() == 2 {
                                    let key = if let Value::BulkString(key_bs) = &values[1] {
                                        String::from_utf8_lossy(key_bs).into_owned()
                                    } else {
                                        return Err("XLEN key must be a bulk string");
                                    };
                                    Ok(Command::XLen { key })
                                } else {
                                    Err("XLEN command expects exactly one argument (key)")
                                }
                            }
                            "XRANGE" => {
                                // XRANGE key start end [COUNT n]
                                if values.len() != 4 && values.len() != 6 {
                                    return Err(
                                        "XRANGE command expects key start end [COUNT n]",
                                    );
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("XRANGE key must be a bulk string");
                                };
                                let start = if let Value::BulkString(s_bs) = &values[2] {
                                    String::from_utf8_lossy(s_bs).into_owned()
                                } else {
                                    return Err("XRANGE start must be a bulk string");
                                };
                                let end = if let Value::BulkString(e_bs) = &values[3] {
                                    String::from_utf8_lossy(e_bs).into_owned()
                                } else {
                                    return Err("XRANGE end must be a bulk string");
                                };
                                let count = if values.len() == 6 {
                                    let kw = if let Value::BulkString(kw_bs) = &values[4] {
                                        String::from_utf8_lossy(kw_bs).to_uppercase()
                                    } else {
                                        return Err("XRANGE COUNT keyword must be a bulk string");
                                    };
                                    if kw != "COUNT" {
                                        return Err("XRANGE syntax error: expected COUNT");
                                    }
                                    let c = if let Value::BulkString(c_bs) = &values[5] {
                                        String::from_utf8_lossy(c_bs)
                                            .parse::<usize>()
                                            .map_err(|_| "XRANGE COUNT is not a valid integer")?
                                    } else {
                                        return Err("XRANGE COUNT must be a bulk string");
                                    };
                                    Some(c)
                                } else {
                                    None
                                };
                                Ok(Command::XRange {
                                    key,
                                    start,
                                    end,
                                    count,
                                })
                            }
                            "XREVRANGE" => {
                                // XREVRANGE key end start [COUNT n]
                                if values.len() != 4 && values.len() != 6 {
                                    return Err(
                                        "XREVRANGE command expects key end start [COUNT n]",
                                    );
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("XREVRANGE key must be a bulk string");
                                };
                                let end = if let Value::BulkString(e_bs) = &values[2] {
                                    String::from_utf8_lossy(e_bs).into_owned()
                                } else {
                                    return Err("XREVRANGE end must be a bulk string");
                                };
                                let start = if let Value::BulkString(s_bs) = &values[3] {
                                    String::from_utf8_lossy(s_bs).into_owned()
                                } else {
                                    return Err("XREVRANGE start must be a bulk string");
                                };
                                let count = if values.len() == 6 {
                                    let kw = if let Value::BulkString(kw_bs) = &values[4] {
                                        String::from_utf8_lossy(kw_bs).to_uppercase()
                                    } else {
                                        return Err("XREVRANGE COUNT keyword must be a bulk string");
                                    };
                                    if kw != "COUNT" {
                                        return Err("XREVRANGE syntax error: expected COUNT");
                                    }
                                    let c = if let Value::BulkString(c_bs) = &values[5] {
                                        String::from_utf8_lossy(c_bs)
                                            .parse::<usize>()
                                            .map_err(|_| "XREVRANGE COUNT is not a valid integer")?
                                    } else {
                                        return Err("XREVRANGE COUNT must be a bulk string");
                                    };
                                    Some(c)
                                } else {
                                    None
                                };
                                Ok(Command::XRevRange {
                                    key,
                                    end,
                                    start,
                                    count,
                                })
                            }
                            "XDEL" => {
                                // XDEL key id [id ...]
                                if values.len() < 3 {
                                    return Err(
                                        "XDEL command expects key id [id ...]",
                                    );
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("XDEL key must be a bulk string");
                                };
                                let mut ids = Vec::with_capacity(values.len() - 2);
                                for v in &values[2..] {
                                    if let Value::BulkString(id_bs) = v {
                                        ids.push(String::from_utf8_lossy(id_bs).into_owned());
                                    } else {
                                        return Err("XDEL ids must be bulk strings");
                                    }
                                }
                                Ok(Command::XDel { key, ids })
                            }
                            "XTRIM" => {
                                // XTRIM key MAXLEN [=|~] count
                                if values.len() < 4 {
                                    return Err(
                                        "XTRIM command expects key MAXLEN [=|~] count",
                                    );
                                }
                                let key = if let Value::BulkString(key_bs) = &values[1] {
                                    String::from_utf8_lossy(key_bs).into_owned()
                                } else {
                                    return Err("XTRIM key must be a bulk string");
                                };
                                let strategy = if let Value::BulkString(s_bs) = &values[2] {
                                    String::from_utf8_lossy(s_bs).to_uppercase()
                                } else {
                                    return Err("XTRIM strategy must be a bulk string");
                                };
                                if strategy != "MAXLEN" {
                                    return Err("XTRIM syntax error: expected MAXLEN");
                                }
                                let mut i = 3;
                                // Optional approximate/exact marker (= or ~); ignore which.
                                let marker = if let Value::BulkString(m_bs) = &values[i] {
                                    String::from_utf8_lossy(m_bs).into_owned()
                                } else {
                                    return Err("XTRIM count must be a bulk string");
                                };
                                if marker == "=" || marker == "~" {
                                    i += 1;
                                }
                                if i >= values.len() {
                                    return Err("XTRIM MAXLEN requires a count");
                                }
                                let maxlen = if let Value::BulkString(c_bs) = &values[i] {
                                    String::from_utf8_lossy(c_bs)
                                        .parse::<usize>()
                                        .map_err(|_| "XTRIM count is not a valid integer")?
                                } else {
                                    return Err("XTRIM count must be a bulk string");
                                };
                                i += 1;
                                if i != values.len() {
                                    return Err("XTRIM command expects key MAXLEN [=|~] count");
                                }
                                Ok(Command::XTrim { key, maxlen })
                            }
                            "XREAD" => {
                                // XREAD [COUNT n] STREAMS key [key ...] id [id ...]
                                let mut count: Option<usize> = None;
                                let mut i = 1;

                                // Optional leading COUNT n (BLOCK is not supported in this phase).
                                while i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err("XREAD arguments must be bulk strings");
                                    };
                                    match token.as_str() {
                                        "COUNT" => {
                                            if i + 1 >= values.len() {
                                                return Err("XREAD COUNT requires a count");
                                            }
                                            let c = if let Value::BulkString(c_bs) = &values[i + 1] {
                                                String::from_utf8_lossy(c_bs)
                                                    .parse::<usize>()
                                                    .map_err(|_| "XREAD COUNT is not a valid integer")?
                                            } else {
                                                return Err("XREAD COUNT must be a bulk string");
                                            };
                                            count = Some(c);
                                            i += 2;
                                        }
                                        "BLOCK" => {
                                            return Err("ERR XREAD BLOCK is not supported yet");
                                        }
                                        "STREAMS" => break,
                                        _ => {
                                            return Err("XREAD syntax error: expected COUNT or STREAMS");
                                        }
                                    }
                                }

                                // The STREAMS keyword must be present.
                                if i >= values.len() {
                                    return Err("XREAD requires the STREAMS keyword");
                                }
                                let streams_kw = if let Value::BulkString(bs) = &values[i] {
                                    String::from_utf8_lossy(bs).to_uppercase()
                                } else {
                                    return Err("XREAD arguments must be bulk strings");
                                };
                                if streams_kw != "STREAMS" {
                                    return Err("XREAD requires the STREAMS keyword");
                                }
                                i += 1;

                                // Remaining args split in half: first half keys, second half ids.
                                let remaining = values.len() - i;
                                if remaining == 0 {
                                    return Err(
                                        "XREAD STREAMS requires at least one key and one id",
                                    );
                                }
                                if !remaining.is_multiple_of(2) {
                                    return Err(
                                        "ERR Unbalanced XREAD list of streams: for each stream key an ID or '$' must be specified.",
                                    );
                                }
                                let n = remaining / 2;
                                let mut streams: Vec<(String, String)> = Vec::with_capacity(n);
                                for j in 0..n {
                                    let key = if let Value::BulkString(k_bs) = &values[i + j] {
                                        String::from_utf8_lossy(k_bs).into_owned()
                                    } else {
                                        return Err("XREAD stream key must be a bulk string");
                                    };
                                    let id = if let Value::BulkString(id_bs) = &values[i + n + j] {
                                        String::from_utf8_lossy(id_bs).into_owned()
                                    } else {
                                        return Err("XREAD stream id must be a bulk string");
                                    };
                                    streams.push((key, id));
                                }
                                Ok(Command::XRead { count, streams })
                            }
                            "XGROUP" => {
                                // XGROUP <subcommand> ...
                                if values.len() < 2 {
                                    return Err(
                                        "ERR wrong number of arguments for 'xgroup' command",
                                    );
                                }
                                let sub = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).to_uppercase()
                                } else {
                                    return Err("XGROUP subcommand must be a bulk string");
                                };
                                match sub.as_str() {
                                    "CREATE" => {
                                        // XGROUP CREATE key group id [MKSTREAM]
                                        if values.len() != 5 && values.len() != 6 {
                                            return Err(
                                                "ERR wrong number of arguments for 'xgroup' command",
                                            );
                                        }
                                        let key = if let Value::BulkString(bs) = &values[2] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP CREATE key must be a bulk string");
                                        };
                                        let group = if let Value::BulkString(bs) = &values[3] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP CREATE group must be a bulk string");
                                        };
                                        let id = if let Value::BulkString(bs) = &values[4] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP CREATE id must be a bulk string");
                                        };
                                        let mkstream = if values.len() == 6 {
                                            let kw = if let Value::BulkString(bs) = &values[5] {
                                                String::from_utf8_lossy(bs).to_uppercase()
                                            } else {
                                                return Err("XGROUP CREATE MKSTREAM must be a bulk string");
                                            };
                                            if kw != "MKSTREAM" {
                                                return Err("XGROUP CREATE syntax error: expected MKSTREAM");
                                            }
                                            true
                                        } else {
                                            false
                                        };
                                        Ok(Command::XGroupCreate {
                                            key,
                                            group,
                                            id,
                                            mkstream,
                                        })
                                    }
                                    "SETID" => {
                                        // XGROUP SETID key group id [ENTRIESREAD n]
                                        if values.len() != 5 && values.len() != 7 {
                                            return Err(
                                                "ERR wrong number of arguments for 'xgroup' command",
                                            );
                                        }
                                        let key = if let Value::BulkString(bs) = &values[2] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP SETID key must be a bulk string");
                                        };
                                        let group = if let Value::BulkString(bs) = &values[3] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP SETID group must be a bulk string");
                                        };
                                        let id = if let Value::BulkString(bs) = &values[4] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP SETID id must be a bulk string");
                                        };
                                        if values.len() == 7 {
                                            let kw = if let Value::BulkString(bs) = &values[5] {
                                                String::from_utf8_lossy(bs).to_uppercase()
                                            } else {
                                                return Err("XGROUP SETID ENTRIESREAD must be a bulk string");
                                            };
                                            if kw != "ENTRIESREAD" {
                                                return Err("XGROUP SETID syntax error: expected ENTRIESREAD");
                                            }
                                        }
                                        Ok(Command::XGroupSetId { key, group, id })
                                    }
                                    "DESTROY" => {
                                        // XGROUP DESTROY key group
                                        if values.len() != 4 {
                                            return Err(
                                                "ERR wrong number of arguments for 'xgroup' command",
                                            );
                                        }
                                        let key = if let Value::BulkString(bs) = &values[2] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP DESTROY key must be a bulk string");
                                        };
                                        let group = if let Value::BulkString(bs) = &values[3] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP DESTROY group must be a bulk string");
                                        };
                                        Ok(Command::XGroupDestroy { key, group })
                                    }
                                    "CREATECONSUMER" => {
                                        // XGROUP CREATECONSUMER key group consumer
                                        if values.len() != 5 {
                                            return Err(
                                                "ERR wrong number of arguments for 'xgroup' command",
                                            );
                                        }
                                        let key = if let Value::BulkString(bs) = &values[2] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP CREATECONSUMER key must be a bulk string");
                                        };
                                        let group = if let Value::BulkString(bs) = &values[3] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP CREATECONSUMER group must be a bulk string");
                                        };
                                        let consumer = if let Value::BulkString(bs) = &values[4] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP CREATECONSUMER consumer must be a bulk string");
                                        };
                                        Ok(Command::XGroupCreateConsumer {
                                            key,
                                            group,
                                            consumer,
                                        })
                                    }
                                    "DELCONSUMER" => {
                                        // XGROUP DELCONSUMER key group consumer
                                        if values.len() != 5 {
                                            return Err(
                                                "ERR wrong number of arguments for 'xgroup' command",
                                            );
                                        }
                                        let key = if let Value::BulkString(bs) = &values[2] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP DELCONSUMER key must be a bulk string");
                                        };
                                        let group = if let Value::BulkString(bs) = &values[3] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP DELCONSUMER group must be a bulk string");
                                        };
                                        let consumer = if let Value::BulkString(bs) = &values[4] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XGROUP DELCONSUMER consumer must be a bulk string");
                                        };
                                        Ok(Command::XGroupDelConsumer {
                                            key,
                                            group,
                                            consumer,
                                        })
                                    }
                                    _ => Ok(Command::Unknown(command_str.to_string())),
                                }
                            }
                            "XREADGROUP" => {
                                // XREADGROUP GROUP g c [COUNT n] [BLOCK ms] [NOACK] STREAMS key... id...
                                if values.len() < 4 {
                                    return Err(
                                        "ERR wrong number of arguments for 'xreadgroup' command",
                                    );
                                }
                                let group_kw = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).to_uppercase()
                                } else {
                                    return Err("XREADGROUP arguments must be bulk strings");
                                };
                                if group_kw != "GROUP" {
                                    return Err("ERR syntax error");
                                }
                                let group = if let Value::BulkString(bs) = &values[2] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XREADGROUP group must be a bulk string");
                                };
                                let consumer = if let Value::BulkString(bs) = &values[3] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XREADGROUP consumer must be a bulk string");
                                };

                                let mut count: Option<usize> = None;
                                let mut noack = false;
                                let mut i = 4;

                                // Optional COUNT n / BLOCK ms / NOACK before STREAMS.
                                while i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err("XREADGROUP arguments must be bulk strings");
                                    };
                                    match token.as_str() {
                                        "COUNT" => {
                                            if i + 1 >= values.len() {
                                                return Err("XREADGROUP COUNT requires a count");
                                            }
                                            let c = if let Value::BulkString(c_bs) = &values[i + 1] {
                                                String::from_utf8_lossy(c_bs)
                                                    .parse::<usize>()
                                                    .map_err(|_| "XREADGROUP COUNT is not a valid integer")?
                                            } else {
                                                return Err("XREADGROUP COUNT must be a bulk string");
                                            };
                                            count = Some(c);
                                            i += 2;
                                        }
                                        "BLOCK" => {
                                            return Err("ERR XREADGROUP BLOCK is not supported yet");
                                        }
                                        "NOACK" => {
                                            noack = true;
                                            i += 1;
                                        }
                                        "STREAMS" => break,
                                        _ => {
                                            return Err("ERR syntax error");
                                        }
                                    }
                                }

                                // The STREAMS keyword must be present.
                                if i >= values.len() {
                                    return Err("ERR syntax error");
                                }
                                let streams_kw = if let Value::BulkString(bs) = &values[i] {
                                    String::from_utf8_lossy(bs).to_uppercase()
                                } else {
                                    return Err("XREADGROUP arguments must be bulk strings");
                                };
                                if streams_kw != "STREAMS" {
                                    return Err("ERR syntax error");
                                }
                                i += 1;

                                // Remaining args split in half: first half keys, second half ids.
                                let remaining = values.len() - i;
                                if remaining == 0 || !remaining.is_multiple_of(2) {
                                    return Err(
                                        "ERR Unbalanced 'xreadgroup' list of streams: for each stream key an ID or '>' must be specified.",
                                    );
                                }
                                let n = remaining / 2;
                                let mut streams: Vec<(String, String)> = Vec::with_capacity(n);
                                for j in 0..n {
                                    let key = if let Value::BulkString(k_bs) = &values[i + j] {
                                        String::from_utf8_lossy(k_bs).into_owned()
                                    } else {
                                        return Err("XREADGROUP stream key must be a bulk string");
                                    };
                                    let id = if let Value::BulkString(id_bs) = &values[i + n + j] {
                                        String::from_utf8_lossy(id_bs).into_owned()
                                    } else {
                                        return Err("XREADGROUP stream id must be a bulk string");
                                    };
                                    streams.push((key, id));
                                }
                                Ok(Command::XReadGroup {
                                    group,
                                    consumer,
                                    count,
                                    noack,
                                    streams,
                                })
                            }
                            "XACK" => {
                                // XACK key group id [id ...]
                                if values.len() < 4 {
                                    return Err(
                                        "ERR wrong number of arguments for 'xack' command",
                                    );
                                }
                                let key = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XACK key must be a bulk string");
                                };
                                let group = if let Value::BulkString(bs) = &values[2] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XACK group must be a bulk string");
                                };
                                let mut ids = Vec::with_capacity(values.len() - 3);
                                for v in &values[3..] {
                                    if let Value::BulkString(id_bs) = v {
                                        ids.push(String::from_utf8_lossy(id_bs).into_owned());
                                    } else {
                                        return Err("XACK ids must be bulk strings");
                                    }
                                }
                                Ok(Command::XAck { key, group, ids })
                            }
                            "XPENDING" => {
                                // XPENDING key group [[IDLE ms] start end count [consumer]]
                                if values.len() < 3 {
                                    return Err(
                                        "ERR wrong number of arguments for 'xpending' command",
                                    );
                                }
                                let key = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XPENDING key must be a bulk string");
                                };
                                let group = if let Value::BulkString(bs) = &values[2] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XPENDING group must be a bulk string");
                                };

                                if values.len() == 3 {
                                    // Summary form.
                                    return Ok(Command::XPending {
                                        key,
                                        group,
                                        start: None,
                                        end: None,
                                        count: None,
                                        consumer: None,
                                        idle: None,
                                    });
                                }

                                // Extended form: [IDLE ms] start end count [consumer]
                                let mut i = 3;
                                let mut idle: Option<u64> = None;
                                let first = if let Value::BulkString(bs) = &values[i] {
                                    String::from_utf8_lossy(bs).to_uppercase()
                                } else {
                                    return Err("XPENDING arguments must be bulk strings");
                                };
                                if first == "IDLE" {
                                    if i + 1 >= values.len() {
                                        return Err("ERR syntax error");
                                    }
                                    let ms = if let Value::BulkString(ms_bs) = &values[i + 1] {
                                        String::from_utf8_lossy(ms_bs)
                                            .parse::<u64>()
                                            .map_err(|_| "ERR syntax error")?
                                    } else {
                                        return Err("XPENDING IDLE must be a bulk string");
                                    };
                                    idle = Some(ms);
                                    i += 2;
                                }

                                // Require start, end, count.
                                if i + 3 > values.len() {
                                    return Err("ERR syntax error");
                                }
                                let start = if let Value::BulkString(bs) = &values[i] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XPENDING start must be a bulk string");
                                };
                                let end = if let Value::BulkString(bs) = &values[i + 1] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XPENDING end must be a bulk string");
                                };
                                let count = if let Value::BulkString(bs) = &values[i + 2] {
                                    String::from_utf8_lossy(bs)
                                        .parse::<usize>()
                                        .map_err(|_| "ERR syntax error")?
                                } else {
                                    return Err("XPENDING count must be a bulk string");
                                };
                                i += 3;

                                // Optional trailing consumer.
                                let consumer = if i < values.len() {
                                    let c = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).into_owned()
                                    } else {
                                        return Err("XPENDING consumer must be a bulk string");
                                    };
                                    i += 1;
                                    Some(c)
                                } else {
                                    None
                                };
                                if i != values.len() {
                                    return Err("ERR syntax error");
                                }
                                Ok(Command::XPending {
                                    key,
                                    group,
                                    start: Some(start),
                                    end: Some(end),
                                    count: Some(count),
                                    consumer,
                                    idle,
                                })
                            }
                            "XCLAIM" => {
                                // XCLAIM key group consumer min-idle-time id [id ...]
                                //   [IDLE ms] [TIME ms] [RETRYCOUNT n] [FORCE] [JUSTID] [LASTID id]
                                if values.len() < 6 {
                                    return Err(
                                        "ERR wrong number of arguments for 'xclaim' command",
                                    );
                                }
                                let key = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XCLAIM key must be a bulk string");
                                };
                                let group = if let Value::BulkString(bs) = &values[2] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XCLAIM group must be a bulk string");
                                };
                                let consumer = if let Value::BulkString(bs) = &values[3] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XCLAIM consumer must be a bulk string");
                                };
                                let min_idle = if let Value::BulkString(bs) = &values[4] {
                                    String::from_utf8_lossy(bs)
                                        .parse::<u64>()
                                        .map_err(|_| "XCLAIM min-idle-time is not a valid integer")?
                                } else {
                                    return Err("XCLAIM min-idle-time must be a bulk string");
                                };

                                // Collect ids until an option keyword is encountered.
                                let mut i = 5;
                                let mut ids: Vec<String> = Vec::new();
                                while i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err("XCLAIM arguments must be bulk strings");
                                    };
                                    match token.as_str() {
                                        "IDLE" | "TIME" | "RETRYCOUNT" | "FORCE" | "JUSTID"
                                        | "LASTID" => break,
                                        _ => {
                                            let id = if let Value::BulkString(bs) = &values[i] {
                                                String::from_utf8_lossy(bs).into_owned()
                                            } else {
                                                return Err("XCLAIM id must be a bulk string");
                                            };
                                            ids.push(id);
                                            i += 1;
                                        }
                                    }
                                }
                                if ids.is_empty() {
                                    return Err(
                                        "ERR wrong number of arguments for 'xclaim' command",
                                    );
                                }

                                let mut idle: Option<u64> = None;
                                let mut time: Option<u64> = None;
                                let mut retrycount: Option<u64> = None;
                                let mut force = false;
                                let mut justid = false;

                                // Parse trailing options.
                                while i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err("XCLAIM arguments must be bulk strings");
                                    };
                                    match token.as_str() {
                                        "IDLE" => {
                                            if i + 1 >= values.len() {
                                                return Err("XCLAIM IDLE requires an argument");
                                            }
                                            let ms = if let Value::BulkString(bs) = &values[i + 1] {
                                                String::from_utf8_lossy(bs)
                                                    .parse::<u64>()
                                                    .map_err(|_| "XCLAIM IDLE is not a valid integer")?
                                            } else {
                                                return Err("XCLAIM IDLE must be a bulk string");
                                            };
                                            idle = Some(ms);
                                            i += 2;
                                        }
                                        "TIME" => {
                                            if i + 1 >= values.len() {
                                                return Err("XCLAIM TIME requires an argument");
                                            }
                                            let ms = if let Value::BulkString(bs) = &values[i + 1] {
                                                String::from_utf8_lossy(bs)
                                                    .parse::<u64>()
                                                    .map_err(|_| "XCLAIM TIME is not a valid integer")?
                                            } else {
                                                return Err("XCLAIM TIME must be a bulk string");
                                            };
                                            time = Some(ms);
                                            i += 2;
                                        }
                                        "RETRYCOUNT" => {
                                            if i + 1 >= values.len() {
                                                return Err("XCLAIM RETRYCOUNT requires an argument");
                                            }
                                            let n = if let Value::BulkString(bs) = &values[i + 1] {
                                                String::from_utf8_lossy(bs)
                                                    .parse::<u64>()
                                                    .map_err(|_| "XCLAIM RETRYCOUNT is not a valid integer")?
                                            } else {
                                                return Err("XCLAIM RETRYCOUNT must be a bulk string");
                                            };
                                            retrycount = Some(n);
                                            i += 2;
                                        }
                                        "FORCE" => {
                                            force = true;
                                            i += 1;
                                        }
                                        "JUSTID" => {
                                            justid = true;
                                            i += 1;
                                        }
                                        "LASTID" => {
                                            if i + 1 >= values.len() {
                                                return Err("XCLAIM LASTID requires an argument");
                                            }
                                            // Consume and ignore the LASTID id.
                                            i += 2;
                                        }
                                        _ => {
                                            return Err("ERR syntax error");
                                        }
                                    }
                                }
                                Ok(Command::XClaim {
                                    key,
                                    group,
                                    consumer,
                                    min_idle,
                                    ids,
                                    idle,
                                    time,
                                    retrycount,
                                    force,
                                    justid,
                                })
                            }
                            "XAUTOCLAIM" => {
                                // XAUTOCLAIM key group consumer min-idle-time start [COUNT n] [JUSTID]
                                if values.len() < 6 {
                                    return Err(
                                        "ERR wrong number of arguments for 'xautoclaim' command",
                                    );
                                }
                                let key = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XAUTOCLAIM key must be a bulk string");
                                };
                                let group = if let Value::BulkString(bs) = &values[2] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XAUTOCLAIM group must be a bulk string");
                                };
                                let consumer = if let Value::BulkString(bs) = &values[3] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XAUTOCLAIM consumer must be a bulk string");
                                };
                                let min_idle = if let Value::BulkString(bs) = &values[4] {
                                    String::from_utf8_lossy(bs)
                                        .parse::<u64>()
                                        .map_err(|_| "XAUTOCLAIM min-idle-time is not a valid integer")?
                                } else {
                                    return Err("XAUTOCLAIM min-idle-time must be a bulk string");
                                };
                                let start = if let Value::BulkString(bs) = &values[5] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XAUTOCLAIM start must be a bulk string");
                                };

                                let mut count: Option<usize> = None;
                                let mut justid = false;
                                let mut i = 6;
                                while i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err("XAUTOCLAIM arguments must be bulk strings");
                                    };
                                    match token.as_str() {
                                        "COUNT" => {
                                            if i + 1 >= values.len() {
                                                return Err("XAUTOCLAIM COUNT requires a count");
                                            }
                                            let c = if let Value::BulkString(c_bs) = &values[i + 1] {
                                                String::from_utf8_lossy(c_bs)
                                                    .parse::<usize>()
                                                    .map_err(|_| "XAUTOCLAIM COUNT is not a valid integer")?
                                            } else {
                                                return Err("XAUTOCLAIM COUNT must be a bulk string");
                                            };
                                            count = Some(c);
                                            i += 2;
                                        }
                                        "JUSTID" => {
                                            justid = true;
                                            i += 1;
                                        }
                                        _ => {
                                            return Err("ERR syntax error");
                                        }
                                    }
                                }
                                Ok(Command::XAutoClaim {
                                    key,
                                    group,
                                    consumer,
                                    min_idle,
                                    start,
                                    count,
                                    justid,
                                })
                            }
                            "XINFO" => {
                                // XINFO <subcommand> ...
                                if values.len() < 2 {
                                    return Err(
                                        "ERR wrong number of arguments for 'xinfo' command",
                                    );
                                }
                                let sub = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).to_uppercase()
                                } else {
                                    return Err("XINFO subcommand must be a bulk string");
                                };
                                match sub.as_str() {
                                    "STREAM" => {
                                        // XINFO STREAM key
                                        if values.len() != 3 {
                                            return Err(
                                                "ERR wrong number of arguments for 'xinfo' command",
                                            );
                                        }
                                        let key = if let Value::BulkString(bs) = &values[2] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XINFO STREAM key must be a bulk string");
                                        };
                                        Ok(Command::XInfoStream { key })
                                    }
                                    "GROUPS" => {
                                        // XINFO GROUPS key
                                        if values.len() != 3 {
                                            return Err(
                                                "ERR wrong number of arguments for 'xinfo' command",
                                            );
                                        }
                                        let key = if let Value::BulkString(bs) = &values[2] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XINFO GROUPS key must be a bulk string");
                                        };
                                        Ok(Command::XInfoGroups { key })
                                    }
                                    "CONSUMERS" => {
                                        // XINFO CONSUMERS key group
                                        if values.len() != 4 {
                                            return Err(
                                                "ERR wrong number of arguments for 'xinfo' command",
                                            );
                                        }
                                        let key = if let Value::BulkString(bs) = &values[2] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XINFO CONSUMERS key must be a bulk string");
                                        };
                                        let group = if let Value::BulkString(bs) = &values[3] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("XINFO CONSUMERS group must be a bulk string");
                                        };
                                        Ok(Command::XInfoConsumers { key, group })
                                    }
                                    _ => Ok(Command::Unknown(command_str.to_string())),
                                }
                            }
                            "XSETID" => {
                                // XSETID key id [ENTRIESADDED n] [MAXDELETEDID id]
                                if values.len() < 3 {
                                    return Err(
                                        "ERR wrong number of arguments for 'xsetid' command",
                                    );
                                }
                                let key = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XSETID key must be a bulk string");
                                };
                                let id = if let Value::BulkString(bs) = &values[2] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("XSETID id must be a bulk string");
                                };

                                let mut entries_added: Option<u64> = None;
                                let mut max_deleted_id: Option<String> = None;
                                let mut i = 3;
                                while i < values.len() {
                                    let token = if let Value::BulkString(bs) = &values[i] {
                                        String::from_utf8_lossy(bs).to_uppercase()
                                    } else {
                                        return Err("XSETID arguments must be bulk strings");
                                    };
                                    match token.as_str() {
                                        "ENTRIESADDED" => {
                                            if i + 1 >= values.len() {
                                                return Err("XSETID ENTRIESADDED requires an argument");
                                            }
                                            let n = if let Value::BulkString(bs) = &values[i + 1] {
                                                String::from_utf8_lossy(bs)
                                                    .parse::<u64>()
                                                    .map_err(|_| "XSETID ENTRIESADDED is not a valid integer")?
                                            } else {
                                                return Err("XSETID ENTRIESADDED must be a bulk string");
                                            };
                                            entries_added = Some(n);
                                            i += 2;
                                        }
                                        "MAXDELETEDID" => {
                                            if i + 1 >= values.len() {
                                                return Err("XSETID MAXDELETEDID requires an argument");
                                            }
                                            let mid = if let Value::BulkString(bs) = &values[i + 1] {
                                                String::from_utf8_lossy(bs).into_owned()
                                            } else {
                                                return Err("XSETID MAXDELETEDID must be a bulk string");
                                            };
                                            max_deleted_id = Some(mid);
                                            i += 2;
                                        }
                                        _ => {
                                            return Err("ERR syntax error");
                                        }
                                    }
                                }
                                Ok(Command::XSetId {
                                    key,
                                    id,
                                    entries_added,
                                    max_deleted_id,
                                })
                            }

                            // Scripting: EVAL/EVAL_RO and EVALSHA/EVALSHA_RO share
                            // a layout: <body|sha> numkeys [key ...] [arg ...].
                            "EVAL" | "EVAL_RO" | "EVALSHA" | "EVALSHA_RO" => {
                                if values.len() < 3 {
                                    return Err(
                                        "ERR wrong number of arguments for 'eval' command",
                                    );
                                }
                                let head = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).into_owned()
                                } else {
                                    return Err("EVAL first argument must be a bulk string");
                                };
                                let numkeys = if let Value::BulkString(bs) = &values[2] {
                                    String::from_utf8_lossy(bs)
                                        .parse::<i64>()
                                        .map_err(|_| "ERR value is not an integer or out of range")?
                                } else {
                                    return Err("EVAL numkeys must be a bulk string");
                                };
                                if numkeys < 0 {
                                    return Err("ERR Number of keys can't be negative");
                                }
                                let numkeys = numkeys as usize;
                                if 3 + numkeys > values.len() {
                                    return Err("ERR Number of keys can't be greater than number of args");
                                }
                                let mut keys: Vec<Vec<u8>> = Vec::with_capacity(numkeys);
                                for v in &values[3..3 + numkeys] {
                                    if let Value::BulkString(bs) = v {
                                        keys.push(bs.clone());
                                    } else {
                                        return Err("EVAL keys must be bulk strings");
                                    }
                                }
                                let mut args: Vec<Vec<u8>> =
                                    Vec::with_capacity(values.len() - (3 + numkeys));
                                for v in &values[3 + numkeys..] {
                                    if let Value::BulkString(bs) = v {
                                        args.push(bs.clone());
                                    } else {
                                        return Err("EVAL args must be bulk strings");
                                    }
                                }
                                if command_str == "EVALSHA" || command_str == "EVALSHA_RO" {
                                    Ok(Command::EvalSha {
                                        sha: head.to_lowercase(),
                                        numkeys,
                                        keys,
                                        args,
                                    })
                                } else {
                                    Ok(Command::Eval {
                                        source: head,
                                        numkeys,
                                        keys,
                                        args,
                                    })
                                }
                            }
                            "SCRIPT" => {
                                if values.len() < 2 {
                                    return Err(
                                        "ERR wrong number of arguments for 'script' command",
                                    );
                                }
                                let sub = if let Value::BulkString(bs) = &values[1] {
                                    String::from_utf8_lossy(bs).to_uppercase()
                                } else {
                                    return Err("SCRIPT subcommand must be a bulk string");
                                };
                                let sub = match sub.as_str() {
                                    "LOAD" => {
                                        if values.len() != 3 {
                                            return Err(
                                                "ERR wrong number of arguments for 'script|load' command",
                                            );
                                        }
                                        let source = if let Value::BulkString(bs) = &values[2] {
                                            String::from_utf8_lossy(bs).into_owned()
                                        } else {
                                            return Err("SCRIPT LOAD body must be a bulk string");
                                        };
                                        ScriptSubCommand::Load { source }
                                    }
                                    "EXISTS" => {
                                        let mut shas = Vec::with_capacity(values.len() - 2);
                                        for v in &values[2..] {
                                            if let Value::BulkString(bs) = v {
                                                shas.push(
                                                    String::from_utf8_lossy(bs).to_lowercase(),
                                                );
                                            } else {
                                                return Err("SCRIPT EXISTS args must be bulk strings");
                                            }
                                        }
                                        ScriptSubCommand::Exists { shas }
                                    }
                                    "FLUSH" => ScriptSubCommand::Flush,
                                    _ => ScriptSubCommand::NoOp,
                                };
                                Ok(Command::Script { sub })
                            }

                            // ---- Admin / introspection (Phase 13) ----
                            "DEBUG" => {
                                let args: Vec<String> = values[1..]
                                    .iter()
                                    .map(|v| match v {
                                        Value::BulkString(b) => {
                                            String::from_utf8_lossy(b).into_owned()
                                        }
                                        _ => String::new(),
                                    })
                                    .collect();
                                Ok(Command::Debug { args })
                            }
                            "RESET" => Ok(Command::Reset {}),
                            "SLOWLOG" => {
                                let sub = values
                                    .get(1)
                                    .and_then(|v| match v {
                                        Value::BulkString(b) => {
                                            Some(String::from_utf8_lossy(b).to_uppercase())
                                        }
                                        _ => None,
                                    })
                                    .unwrap_or_default();
                                let arg = values.get(2).and_then(|v| match v {
                                    Value::BulkString(b) => {
                                        Some(String::from_utf8_lossy(b).into_owned())
                                    }
                                    _ => None,
                                });
                                Ok(Command::SlowLog { sub, arg })
                            }
                            "MEMORY" => {
                                let sub = values
                                    .get(1)
                                    .and_then(|v| match v {
                                        Value::BulkString(b) => {
                                            Some(String::from_utf8_lossy(b).to_uppercase())
                                        }
                                        _ => None,
                                    })
                                    .unwrap_or_default();
                                let key = values.get(2).and_then(|v| match v {
                                    Value::BulkString(b) => {
                                        Some(String::from_utf8_lossy(b).into_owned())
                                    }
                                    _ => None,
                                });
                                Ok(Command::Memory { sub, key })
                            }
                            "LATENCY" => {
                                let sub = values
                                    .get(1)
                                    .and_then(|v| match v {
                                        Value::BulkString(b) => {
                                            Some(String::from_utf8_lossy(b).to_uppercase())
                                        }
                                        _ => None,
                                    })
                                    .unwrap_or_default();
                                Ok(Command::Latency { sub })
                            }
                            "LOLWUT" => Ok(Command::Lolwut {}),
                            "ACL" => {
                                let args: Vec<String> = values[1..]
                                    .iter()
                                    .map(|v| match v {
                                        Value::BulkString(b) => {
                                            String::from_utf8_lossy(b).into_owned()
                                        }
                                        _ => String::new(),
                                    })
                                    .collect();
                                Ok(Command::Acl { args })
                            }
                            "FUNCTION" => {
                                let args: Vec<String> = values[1..]
                                    .iter()
                                    .map(|v| match v {
                                        Value::BulkString(b) => {
                                            String::from_utf8_lossy(b).into_owned()
                                        }
                                        _ => String::new(),
                                    })
                                    .collect();
                                Ok(Command::Function { args })
                            }
                            "WAITAOF" => {
                                let parse_i = |idx: usize| -> i64 {
                                    values
                                        .get(idx)
                                        .and_then(|v| match v {
                                            Value::BulkString(b) => String::from_utf8_lossy(b)
                                                .parse::<i64>()
                                                .ok(),
                                            _ => None,
                                        })
                                        .unwrap_or(0)
                                };
                                Ok(Command::WaitAof {
                                    numlocal: parse_i(1),
                                    numreplicas: parse_i(2),
                                    timeout: parse_i(3),
                                })
                            }
                            "FAILOVER" => Ok(Command::Failover {}),
                            "SAVE" => Ok(Command::Save {}),
                            "BGSAVE" => Ok(Command::BgSave {}),
                            "LASTSAVE" => Ok(Command::LastSave {}),
                            "BGREWRITEAOF" => Ok(Command::BgRewriteAof {}),

                            _ => Ok(Command::Unknown(command_str.to_string())),
                        }
                    }
                    Err(_) => {
                        error!("Failed to parse command string from bulk string: {:?}", bs);
                        Err("Invalid command string encoding")
                    }
                }
            }
            _ => Err("Command must start with a bulk string"),
        }
    }
}
