mod resolver;

pub(crate) use resolver::load_dotenv;

pub use resolver::{
    ResolveMode, resolve_variables, resolve_variables_in, resolve_variables_with, substitute,
    substitute_with_check, substitute_with_check_failed,
};
