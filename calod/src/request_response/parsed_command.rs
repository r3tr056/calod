use crate::{
    parser::parser::{ParseError, RESPOutput},
    request_response::command::Command,
};

#[derive(Debug, PartialEq)]
pub struct ParsedCommand<'a> {
    pub command: Option<Command>,
    pub args: Vec<&'a str>,
}

impl<'a> ParsedCommand<'a> {
    pub fn new() -> ParsedCommand<'a> {
        ParsedCommand {
            command: None,
            args: Vec::new(),
        }
    }

    pub fn command(&self) -> &Option<Command> {
        &self.command
    }

    pub fn args(&self) -> &Vec<&str> {
        &self.args
    }

    pub fn set_command(&mut self, command: Option<Command>) {
        self.command = command;
    }

    pub fn set_args(&mut self, args: Vec<&'a str>) {
        self.args = args;
    }

    pub fn append_args(&mut self, arg: &'a str) {
        self.args.push(arg);
    }

    // pub fn from_resp(resp: RESPOutput) -> Result<ParsedCommand<'a>,
    // ParseError> {
    //     match resp {
    //         RESPOutput::Array(Some(array)) => {
    //             if array.is_empty() {
    //                 return Err(ParseError::InvalidInput);
    //             }

    //             let mut iter = array.iter();
    //             let command_resp = iter.next().ok_or(ParseError::InvalidInput)?;

    //             let command = match command_resp {
    //                 RESPOutput::BulkString(Some(command_str)) => {
    //                     Some(Command::from_str())
    //                 }
    //             }
    //         }
    //     }
    // }
}
