#![warn(clippy::match_same_arms)]

pub enum Abc {
    A,
    B,
    C,
}

fn match_same_arms() {
    let _ = match Abc::A {
        Abc::A => 0,
        Abc::B => 1,
        _ => 0, //~ ERROR match arms have same body
    };

    match (1, 2, 3) {
        (1, .., 3) => 42,
        (.., 3) => 42, //~ ERROR match arms have same body
        _ => 0,
    };

    let _ = match 42 {
        42 => 1,
        51 => 1, //~ ERROR match arms have same body
        41 => 2,
        52 => 2, //~ ERROR match arms have same body
        _ => 0,
    };

    let _ = match 42 {
        1 => 2,
        2 => 2, //~ ERROR 2nd matched arms have same body
        3 => 2, //~ ERROR 3rd matched arms have same body
        4 => 3,
        _ => 0,
    };
}

mod issue4244 {
    #[derive(PartialEq, PartialOrd, Eq, Ord)]
    pub enum CommandInfo {
        BuiltIn { name: String, about: Option<String> },
        External { name: String, path: std::path::PathBuf },
    }

    impl CommandInfo {
        pub fn name(&self) -> String {
            match self {
                CommandInfo::BuiltIn { name, .. } => name.to_string(),
                CommandInfo::External { name, .. } => name.to_string(),
            }
        }
    }
}

macro_rules! m {
    (foo) => {};
    (bar) => {};
}

fn main() {
    let x = 0;
    let _ = match 0 {
        0 => {
            m!(foo);
            x
        },
        1 => {
            m!(bar);
            x
        },
        _ => 1,
    };

    let _ = match 0 {
        0 => {
            m!(foo);
            0
        },
        1 => {
            m!(bar);
            0
        },
        _ => 1,
    };

    let _ = match 0 {
        0 => {
            let mut x = 0;
            #[cfg(not_enabled)]
            {
                x = 5;
            }
            #[cfg(not(not_enabled))]
            {
                x = 6;
            }
            x
        },
        1 => {
            let mut x = 0;
            #[cfg(also_not_enabled)]
            {
                x = 5;
            }
            #[cfg(not(also_not_enabled))]
            {
                x = 6;
            }
            x
        },
        _ => 0,
    };
}
