use std::fmt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RustmakeError {
    #[error("WorkflowError:\n{0}")]
    Workflow(String),

    #[error("WildcardError:\n{0}")]
    Wildcard(String),

    #[error("AmbiguousRuleException:\n{0}")]
    AmbiguousRule(String),

    #[error("CyclicGraphException:\n{0}")]
    CyclicGraph(String),

    #[error("MissingInputException:\n{0}")]
    MissingInput(String),

    #[error("MissingOutputException:\n{0}")]
    MissingOutput(String),

    #[error("MissingRuleException:\n{0}")]
    MissingRule(String),

    #[error("UnknownRuleException:\n{0}")]
    UnknownRule(String),

    #[error("NoRulesException:\n{0}")]
    NoRules(String),

    #[error("ProtectedOutputException:\n{0}")]
    ProtectedOutput(String),

    #[error("ImproperOutputException:\n{0}")]
    ImproperOutput(String),

    #[error("ChildIOException:\n{0}")]
    ChildIO(String),

    #[error("UndefinedPathvarException:\n{0}")]
    UndefinedPathvar(String),

    #[error("LockException:\n{0}")]
    Lock(String),

    #[error("IncompleteFilesException:\n{0}")]
    IncompleteFiles(String),

    #[error("ResourceConversionError:\n{0}")]
    ResourceConversion(String),

    #[error("ResourceDuplicationError:\n{0}")]
    ResourceDuplication(String),

    #[error("ResourceValidationError:\n{0}")]
    ResourceValidation(String),

    #[error("ResourceConstraintError:\n{0}")]
    ResourceConstraint(String),

    #[error("ResourceScopesException:\n{0}")]
    ResourceScopes(String),

    #[error("ResourceInsufficiencyError:\n{0}")]
    ResourceInsufficiency(String),

    #[error("InputFunctionException:\n{0}")]
    InputFunction(String),

    #[error("CreateRuleException:\n{0}")]
    CreateRule(String),

    #[error("SourceFileError:\n{0}")]
    SourceFile(String),

    #[error("RuleException:\n{0}")]
    Rule(String),

    #[error("CliException:\n{0}")]
    Cli(String),

    #[error("SyntaxError:\n{0}")]
    Syntax(String),

    #[error("IOError: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, RustmakeError>;

impl RustmakeError {
    pub fn workflow(msg: impl Into<String>) -> Self {
        Self::Workflow(msg.into())
    }

    pub fn wildcard(msg: impl Into<String>) -> Self {
        Self::Wildcard(msg.into())
    }

    pub fn with_location(
        self,
        rule: Option<&str>,
        snakefile: Option<&str>,
        lineno: Option<usize>,
    ) -> Self {
        let mut loc = String::new();
        if let (Some(sf), Some(ln)) = (snakefile, lineno) {
            if let Some(r) = rule {
                loc = format!(" in rule {} in file \"{}\", line {}", r, sf, ln);
            } else {
                loc = format!(" in file \"{}\", line {}", sf, ln);
            }
        } else if let Some(r) = rule {
            loc = format!(" in rule {}", r);
        }
        if loc.is_empty() {
            return self;
        }
        match self {
            Self::Workflow(m) => Self::Workflow(format!("{loc}:\n{m}")),
            Self::Rule(m) => Self::Rule(format!("{loc}:\n{m}")),
            Self::CreateRule(m) => Self::CreateRule(format!("{loc}:\n{m}")),
            other => other,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Location {
    pub snakefile: Option<String>,
    pub lineno: Option<usize>,
    pub rule: Option<String>,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref r) = self.rule {
            write!(f, "rule {}", r)?;
        }
        if let Some(ref sf) = self.snakefile {
            if self.rule.is_some() {
                write!(f, " ")?;
            }
            write!(f, "\"{}\"", sf)?;
            if let Some(ln) = self.lineno {
                write!(f, ", line {}", ln)?;
            }
        }
        Ok(())
    }
}
