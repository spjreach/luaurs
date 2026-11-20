use luau_ast::lexer::Lexer;
use luau_ast::lexeme::{LexemeData, LexemeType};
use luau_ast::name_table::AstNameTable;

fn main() {
    let mut names = AstNameTable::new();
    let source = if let Some(arg) = std::env::args().nth(1) {
        arg.into_bytes()
    } else {
        b"--line comment\n--[==[ multiline ]==] x".to_vec()
    };
    let source: &[u8] = &source;
    let mut lexer = Lexer::from_source(source, &mut names);
    loop {
        let l = lexer.next();
        let slice = match l.data {
            LexemeData::Slice { start, length } => {
                String::from_utf8_lossy(&source[start as usize..(start + length) as usize])
                    .into_owned()
            }
            LexemeData::Name(n) => format!("name:{n}"),
            _ => String::new(),
        };
        println!("{:?} loc={:?}..{:?} data={:?}", l.ty, l.location.begin, l.location.end, slice);
        if l.ty == LexemeType::Eof {
            break;
        }
    }
}
