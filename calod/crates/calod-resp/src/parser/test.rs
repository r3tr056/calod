
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_string() {
        let buffer = b"+OK\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::SimpleString("OK".to_string()));
        assert_eq!(consumed, 5);
    }

    #[test]
    fn test_parse_error() {
        let buffer = b"-Error message\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Error("Error message".to_string()));
        assert_eq!(consumed, 14);
    }

    #[test]
    fn test_parse_integer() {
        let buffer = b":123\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Integer(123));
        assert_eq!(consumed, 6);
    }

    #[test]
    fn test_parse_bulk_string() {
        let buffer = b"$6\r\nfoobar\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::BulkString(b"foobar".to_vec()));
        assert_eq!(consumed, 12);
    }

    #[test]
    fn test_parse_nil_bulk_string() {
        let buffer = b"$-1\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Nil);
        assert_eq!(consumed, 5);
    }

    #[test]
    fn test_parse_array() {
        let buffer = b"*2\r\n+OK\r\n$5\r\nhello\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Array(vec![
            Value::SimpleString("OK".to_string()),
            Value::BulkString(b"hello".to_vec()),
        ]));
        assert_eq!(consumed, 19);
    }

    #[test]
    fn test_parse_nested_array() {
        let buffer = b"*2\r\n*2\r\n:1\r\n:2\r\n*1\r\n+hello\r\n";
        let (value, consumed) = RESPParser::parse_value(buffer).unwrap();
        assert_eq!(value, Value::Array(vec![
            Value::Array(vec![
                Value::Integer(1),
                Value::Integer(2),
            ]),
            Value::Array(vec![
                Value::SimpleString("hello".to_string()),
            ]),
        ]));
        assert_eq!(consumed, 27);
    }

    #[test]
    fn test_parse_multiple_values() {
        let buffer = b"+PONG\r\n+OK\r\n*2\r\n:1\r\n:2\r\n";
        let (value, consumed) = RESPParser::parse_multiple(buffer).unwrap();
        assert_eq!(value, Value::Array(vec![
            Value::SimpleString("PONG".to_string()),
            Value::SimpleString("OK".to_string()),
            Value::Array(vec![
                Value::Integer(1),
                Value::Integer(2),
            ]),
        ]));
        assert_eq!(consumed, 25);
    }

}