use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, fmt, path::Path};

/// Shell configuration to open the terminal with.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, JsonSchema, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Shell {
    /// Use the system's default terminal configuration in /etc/passwd
    #[default]
    System,
    /// Use a specific program with no arguments.
    Program(String),
    /// Use a specific program with arguments.
    WithArguments {
        /// The program to run.
        program: String,
        /// The arguments to pass to the program.
        args: Vec<String>,
        /// An optional string to override the title of the terminal tab
        title_override: Option<String>,
    },
}

impl Shell {
    pub fn program(&self) -> String {
        match self {
            Shell::Program(program) => program.clone(),
            Shell::WithArguments { program, .. } => program.clone(),
            Shell::System => "/bin/bash".to_string(),
        }
    }

    pub fn program_and_args(&self) -> (String, &[String]) {
        match self {
            Shell::Program(program) => (program.clone(), &[]),
            Shell::WithArguments { program, args, .. } => (program.clone(), args),
            Shell::System => ("/bin/bash".to_string(), &[]),
        }
    }

    pub fn shell_kind(&self) -> ShellKind {
        match self {
            Shell::Program(program) | Shell::WithArguments { program, .. } => {
                ShellKind::new(program)
            }
            Shell::System => ShellKind::new(&"/bin/bash".to_string()),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShellKind {
    #[default]
    Fish,
    Posix,
}

pub fn get_system_shell() -> String {
    "/bin/bash".to_string()
}

pub fn get_default_system_shell() -> String {
    "/bin/bash".to_string()
}

/// Get the default system shell, preferring bash on Windows.
pub fn get_default_system_shell_preferring_bash() -> String {
    "/bin/bash".to_string()
}

impl fmt::Display for ShellKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellKind::Posix => write!(f, "bash"),
            ShellKind::Fish => write!(f, "fish"),
        }
    }
}

impl ShellKind {
    pub fn system() -> Self {
        Self::new(&"/bin/bash".to_string())
    }

    pub fn new(program: impl AsRef<Path>) -> Self {
        let program = program.as_ref();
        let program = program
            .file_stem()
            .unwrap_or_else(|| program.as_os_str())
            .to_string_lossy();

        match &*program {
            "fish" => ShellKind::Fish,

            "bash" => ShellKind::Posix,
            // Some other shell detected, the user might install and use a
            // unix-like shell.
            _ => ShellKind::Posix,
        }
    }

    pub fn args_for_shell(&self, interactive: bool, combined_command: String) -> Vec<String> {
        interactive
            .then(|| "-i".to_owned())
            .into_iter()
            .chain(["-c".to_owned(), combined_command])
            .collect()
    }

    pub fn try_quote<'a>(&self, arg: &'a str) -> Option<Cow<'a, str>> {
        shlex::try_quote(arg).ok()
    }

    /// Quotes the given argument if necessary, taking into account the command prefix.
    ///
    /// In other words, this will consider quoting arg without its command prefix to not break the command.
    /// You should use this over `try_quote` when you want to quote a shell command.
    pub fn try_quote_prefix_aware<'a>(&self, arg: &'a str) -> Option<Cow<'a, str>> {
        self.try_quote(arg).map(|quoted| match quoted {
            unquoted @ Cow::Borrowed(_) => unquoted,
            Cow::Owned(quoted) => Cow::Owned(Cow::Borrowed(&quoted).into_owned()),
        })
    }

    pub fn split(&self, input: &str) -> Option<Vec<String>> {
        shlex::split(input)
    }
}


