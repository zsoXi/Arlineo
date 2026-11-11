//! Fact extraction from one source file (`SPEC.md`, "Extraction").

pub mod rust;
pub mod sql;
pub mod ts;

use crate::config::Config;
use crate::error::Result;
use crate::lex::Lexed;
use crate::model::{Confidence, Fact, FileData, Lang, UnparsedData};
use crate::source::SourceFile;

/// A Rust crate in the repository: directory of its `Cargo.toml` (`""` for the repository
/// root) and package name with `-` replaced by `_`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrateRoot {
    pub dir: String,
    pub name: String,
}

/// Context for extracting one file.
#[derive(Clone, Copy, Debug)]
pub struct ExtractCtx<'a> {
    pub config: &'a Config,
    /// All Rust crates of the repository, for module paths (`rust::rust_location`).
    pub crates: &'a [CrateRoot],
}

/// All facts of one file: exactly one `file` fact, one `unparsed` fact per lexer error, and
/// the facts of the language extractor. Errors only on internal bugs.
pub fn extract(file: &SourceFile, ctx: &ExtractCtx<'_>) -> Result<Vec<Fact>> {
    let mut facts = Vec::new();
    let lines = u32::try_from(file.text.lines().count()).unwrap_or(u32::MAX);
    facts.push(Fact::new(
        &file.path,
        0,
        file.path.as_str(),
        Confidence::Exact,
        &FileData {
            lang: file.lang,
            lines,
            hash: file.hash.clone(),
        },
    )?);
    let lexed = match file.lang {
        Lang::Rust => crate::lex::rust::lex(&file.text),
        Lang::Sql => crate::lex::sql::lex(&file.text),
        Lang::TypeScript | Lang::JavaScript => {
            crate::lex::ts::lex(&file.text, Lang::is_jsx(&file.path))
        }
    };
    push_lex_errors(file, &lexed, &mut facts)?;
    let found = match file.lang {
        Lang::Rust => rust::extract_rust(file, &lexed, ctx)?,
        Lang::Sql => sql::extract_sql(file, &lexed, ctx)?,
        Lang::TypeScript | Lang::JavaScript => ts::extract_ts(file, &lexed, ctx)?,
    };
    facts.extend(found);
    Ok(facts)
}

fn push_lex_errors(file: &SourceFile, lexed: &Lexed, facts: &mut Vec<Fact>) -> Result<()> {
    for error in &lexed.errors {
        facts.push(Fact::new(
            &file.path,
            error.line,
            file.path.as_str(),
            Confidence::Exact,
            &UnparsedData {
                reason: format!("lexer: {}", error.message),
            },
        )?);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::{CrateRoot, ExtractCtx, extract};
    use crate::config::Config;
    use crate::model::{Fact, FactKind, Lang, RelPath};
    use crate::source::SourceFile;

    /// Facts of one file with the default configuration, in a repository whose root is the
    /// crate `app`.
    pub(crate) fn facts(path: &str, text: &str) -> Vec<Fact> {
        let config = Config::defaults().expect("default configuration");
        facts_with(&config, path, text)
    }

    /// Facts of one file with `config`, in a repository whose root is the crate `app`.
    pub(crate) fn facts_with(config: &Config, path: &str, text: &str) -> Vec<Fact> {
        let path = RelPath::new(path).expect("test path");
        let lang = Lang::from_path(&path).expect("supported language");
        let file = SourceFile {
            path,
            lang,
            text: text.to_owned(),
            hash: crate::util::fnv::hex(text.as_bytes()),
        };
        let crates = [CrateRoot {
            dir: String::new(),
            name: "app".to_owned(),
        }];
        extract(
            &file,
            &ExtractCtx {
                config,
                crates: &crates,
            },
        )
        .expect("extract")
    }

    /// Keys of the facts of `kind`, in order.
    pub(crate) fn keys(facts: &[Fact], kind: FactKind) -> Vec<String> {
        facts
            .iter()
            .filter(|fact| fact.kind == kind)
            .map(|fact| fact.key.clone())
            .collect()
    }

    /// `(owner, name)` of every call fact.
    pub(crate) fn calls(facts: &[Fact]) -> Vec<(String, String)> {
        facts
            .iter()
            .filter(|fact| fact.kind == FactKind::Call)
            .map(|fact| {
                (
                    fact.data_text("owner").unwrap_or_default(),
                    fact.data_text("name").unwrap_or_default(),
                )
            })
            .collect()
    }
}
