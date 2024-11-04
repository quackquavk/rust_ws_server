
            /// Returns the `rustc` SemVer version and additional metadata
            /// like the git short hash and build date.
            pub fn version_meta() -> VersionMeta {
                VersionMeta {
                    semver: Version {
                        major: 1,
                        minor: 81,
                        patch: 0,
                        pre: vec![],
                        build: vec![],
                    },
                    host: "aarch64-apple-darwin".to_owned(),
                    short_version_string: "rustc 1.81.0 (eeb90cda1 2024-09-04)".to_owned(),
                    commit_hash: Some("eeb90cda1969383f56a2637cbd3037bdf598841c".to_owned()),
                    commit_date: Some("2024-09-04".to_owned()),
                    build_date: None,
                    channel: Channel::Stable,
                }
            }
            