#![feature(box_syntax, box_patterns)]

pub use sourcemap;
pub use swc_atoms as atoms;
pub use swc_common as common;
pub use swc_ecmascript as ecmascript;

mod builder;
pub mod config;

pub use crate::builder::PassBuilder;
use crate::config::{
    BuiltConfig, Config, ConfigFile, InputSourceMap, JscTarget, Merge, Options, Rc, RootMode,
    SourceMapsConfig,
};
use anyhow::{Context, Error};
use common::{
    comments::{Comment, Comments},
    errors::Handler,
    BytePos, FileName, FoldWith, Globals, SourceFile, SourceMap, Spanned, GLOBALS,
};
use ecmascript::{
    ast::Program,
    codegen::{self, Emitter},
    parser::{lexer::Lexer, Parser, Session as ParseSess, Syntax},
    transforms::{
        helpers::{self, Helpers},
        util,
        util::COMMENTS,
    },
};
pub use ecmascript::{
    parser::SourceFileInput,
    transforms::{chain_at, pass::Pass},
};
use serde::Serialize;
use serde_json::error::Category;
use std::{
    fs::{read_to_string, File},
    path::{Path, PathBuf},
    sync::Arc,
};

pub struct Compiler {
    /// swc uses rustc's span interning.
    ///
    /// The `Globals` struct contains span interner.
    globals: Globals,
    /// CodeMap
    pub cm: Arc<SourceMap>,
    pub handler: Handler,
    comments: Comments,
}

#[derive(Debug, Serialize)]
pub struct TransformOutput {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub map: Option<String>,
}

/// These are **low-level** apis.
impl Compiler {
    pub fn comments(&self) -> &Comments {
        &self.comments
    }

    /// Runs `op` in current compiler's context.
    ///
    /// Note: Other methods of `Compiler` already uses this internally.
    pub fn run<R, F>(&self, op: F) -> R
    where
        F: FnOnce() -> R,
    {
        GLOBALS.set(&self.globals, || {
            //
            COMMENTS.set(&self.comments, || {
                //
                op()
            })
        })
    }

    /// This method parses a javascript / typescript file
    pub fn parse_js(
        &self,
        fm: Arc<SourceFile>,
        target: JscTarget,
        syntax: Syntax,
        is_module: bool,
        parse_comments: bool,
        input_source_map: &InputSourceMap,
    ) -> Result<(Program, Option<sourcemap::SourceMap>), Error> {
        self.run(|| {
            let orig = (|| {
                // Load original source map
                match input_source_map {
                    InputSourceMap::Bool(false) => None,
                    InputSourceMap::Bool(true) => {
                        // Load original source map if possible
                        match &fm.name {
                            FileName::Real(filename) => {
                                let path = format!("{}.map", filename.display());
                                let file = File::open(&path).ok()?;
                                Some(sourcemap::SourceMap::from_reader(file).with_context(|| {
                                    format!("failed to read input source map from file at {}", path)
                                }))
                            }
                            _ => {
                                log::error!("Failed to load source map for non-file input");
                                return None;
                            }
                        }
                    }
                    InputSourceMap::Str(ref s) => {
                        if s == "inline" {
                            // Load inline source map by simple string
                            // operations
                            let s = "sourceMappingURL=data:application/json;base64,";
                            let idx = s.rfind(s)?;
                            let encoded = &s[idx + s.len()..];

                            let res = base64::decode(encoded.as_bytes())
                                .context("failed to decode base64-encoded source map");
                            let res = match res {
                                Ok(v) => v,
                                Err(err) => return Some(Err(err)),
                            };

                            Some(sourcemap::SourceMap::from_slice(&res).context(
                                "failed to read input source map from inlined base64 encoded \
                                 string",
                            ))
                        } else {
                            // Load source map passed by user
                            Some(sourcemap::SourceMap::from_slice(s.as_bytes()).context(
                                "failed to read input source map from user-provided sourcemap",
                            ))
                        }
                    }
                }
            })();

            let orig = match orig {
                None => None,
                Some(v) => Some(v?),
            };

            let session = ParseSess {
                handler: &self.handler,
            };
            let lexer = Lexer::new(
                session,
                syntax,
                target,
                SourceFileInput::from(&*fm),
                if parse_comments {
                    Some(&self.comments)
                } else {
                    None
                },
            );
            let mut parser = Parser::new_from(session, lexer);
            let program = if is_module {
                parser
                    .parse_module()
                    .map_err(|mut e| {
                        e.emit();
                        Error::msg("failed to parse module")
                    })
                    .map(Program::Module)?
            } else {
                parser
                    .parse_script()
                    .map_err(|mut e| {
                        e.emit();
                        Error::msg("failed to parse module")
                    })
                    .map(Program::Script)?
            };

            Ok((program, orig))
        })
    }

    pub fn print(
        &self,
        program: &Program,
        comments: &Comments,
        source_map: SourceMapsConfig,
        orig: Option<&sourcemap::SourceMap>,
        minify: bool,
    ) -> Result<TransformOutput, Error> {
        self.run(|| {
            let mut src_map_buf = vec![];

            let src = {
                let mut buf = vec![];
                {
                    let handlers = box MyHandlers;
                    let mut emitter = Emitter {
                        cfg: codegen::Config { minify },
                        comments: Some(&comments),
                        cm: self.cm.clone(),
                        wr: box codegen::text_writer::JsWriter::new(
                            self.cm.clone(),
                            "\n",
                            &mut buf,
                            if source_map.enabled() {
                                Some(&mut src_map_buf)
                            } else {
                                None
                            },
                        ),
                        handlers,
                    };

                    emitter
                        .emit_program(&program)
                        .context("failed to emit module")?;
                }
                // Invalid utf8 is valid in javascript world.
                unsafe { String::from_utf8_unchecked(buf) }
            };
            let (code, map) = match source_map {
                SourceMapsConfig::Bool(v) => {
                    if v {
                        let mut buf = vec![];

                        self.cm
                            .build_source_map_from(&mut src_map_buf, orig)
                            .to_writer(&mut buf)
                            .context("failed to write source map")?;
                        let map = String::from_utf8(buf).context("source map is not utf-8")?;
                        (src, Some(map))
                    } else {
                        (src, None)
                    }
                }
                SourceMapsConfig::Str(_) => {
                    let mut src = src;

                    let mut buf = vec![];

                    self.cm
                        .build_source_map(&mut src_map_buf)
                        .to_writer(&mut buf)
                        .context("failed to write source map file")?;
                    let map = String::from_utf8(buf).context("source map is not utf-8")?;

                    src.push_str("\n//# sourceMappingURL=data:application/json;base64,");
                    base64::encode_config_buf(
                        map.as_bytes(),
                        base64::Config::new(base64::CharacterSet::UrlSafe, true),
                        &mut src,
                    );
                    (src, None)
                }
            };

            Ok(TransformOutput { code, map })
        })
    }
}

/// High-level apis.
impl Compiler {
    pub fn new(cm: Arc<SourceMap>, handler: Handler) -> Self {
        Compiler {
            cm,
            handler,
            globals: Globals::new(),
            comments: Default::default(),
        }
    }

    /// This method handles merging of config.
    pub fn config_for_file(
        &self,
        opts: &Options,
        name: &FileName,
    ) -> Result<BuiltConfig<impl Pass>, Error> {
        self.run(|| -> Result<_, Error> {
            let Options {
                ref root,
                root_mode,
                swcrc,
                config_file,
                is_module,
                ..
            } = opts;
            let root = root.clone().unwrap_or_else(|| {
                if cfg!(target_arch = "wasm32") {
                    PathBuf::new()
                } else {
                    ::std::env::current_dir().unwrap()
                }
            });

            let config_file = match config_file {
                Some(ConfigFile::Str(ref s)) => Some(load_swcrc(Path::new(&s))?),
                _ => None,
            };

            match name {
                FileName::Real(ref path) => {
                    if *swcrc {
                        let mut parent = path.parent();
                        while let Some(dir) = parent {
                            let swcrc = dir.join(".swcrc");

                            if swcrc.exists() {
                                let config = load_swcrc(&swcrc)?;

                                let mut config = config
                                    .into_config(Some(path))
                                    .context("failed to process config file")?;

                                if let Some(config_file) = config_file {
                                    config.merge(&config_file.into_config(Some(path))?)
                                }
                                let built =
                                    opts.build(&self.cm, &self.handler, *is_module, Some(config));
                                return Ok(built);
                            }

                            if dir == root && *root_mode == RootMode::Root {
                                break;
                            }
                            parent = dir.parent();
                        }
                    }

                    let config_file = config_file.unwrap_or_else(|| Rc::default());
                    let built = opts.build(
                        &self.cm,
                        &self.handler,
                        *is_module,
                        Some(config_file.into_config(Some(path))?),
                    );
                    return Ok(built);
                }
                _ => {}
            }

            let built = opts.build(
                &self.cm,
                &self.handler,
                *is_module,
                match config_file {
                    Some(config_file) => Some(config_file.into_config(None)?),
                    None => Some(Rc::default().into_config(None)?),
                },
            );
            Ok(built)
        })
        .with_context(|| format!("failed to load config for file '{:?}'", name))
    }

    // TODO: Handle source map
    pub fn process_js_file(
        &self,
        fm: Arc<SourceFile>,
        opts: &Options,
    ) -> Result<TransformOutput, Error> {
        self.run(|| -> Result<_, Error> {
            let config = self.run(|| self.config_for_file(opts, &fm.name))?;
            let (program, src_map) = self.parse_js(
                fm.clone(),
                config.target,
                config.syntax,
                config.is_module,
                true,
                &config.input_source_map,
            )?;

            self.process_js_inner(program, src_map, config)
        })
        .context("failed to process js file")
    }

    /// You can use custom pass with this method.
    ///
    /// There exists a [PassBuilder] to help building custom passes.
    pub fn process_js(
        &self,
        program: Program,
        src_map: Option<sourcemap::SourceMap>,
        opts: &Options,
    ) -> Result<TransformOutput, Error> {
        self.run(|| -> Result<_, Error> {
            let loc = self.cm.lookup_char_pos(program.span().lo());
            let fm = loc.file;

            let config = self.run(|| self.config_for_file(opts, &fm.name))?;

            self.process_js_inner(program, src_map, config)
        })
        .context("failed to process js module")
    }

    fn process_js_inner(
        &self,
        program: Program,
        src_map: Option<sourcemap::SourceMap>,
        config: BuiltConfig<impl Pass>,
    ) -> Result<TransformOutput, Error> {
        self.run(|| {
            if config.minify {
                let preserve_excl = |_: &BytePos, vc: &mut Vec<Comment>| -> bool {
                    vc.retain(|c: &Comment| c.text.starts_with("!"));
                    !vc.is_empty()
                };
                self.comments.retain_leading(preserve_excl);
                self.comments.retain_trailing(preserve_excl);
            }
            let mut pass = config.pass;
            let program = helpers::HELPERS.set(&Helpers::new(config.external_helpers), || {
                util::HANDLER.set(&self.handler, || {
                    // Fold module
                    program.fold_with(&mut pass)
                })
            });

            self.print(
                &program,
                &self.comments,
                config.source_maps,
                src_map.as_ref(),
                config.minify,
            )
        })
    }
}

struct MyHandlers;

impl ecmascript::codegen::Handlers for MyHandlers {}

fn load_swcrc(path: &Path) -> Result<Rc, Error> {
    fn convert_json_err(e: serde_json::Error) -> Error {
        let line = e.line();
        let column = e.column();

        let msg = match e.classify() {
            Category::Io => "io error",
            Category::Syntax => "syntax error",
            Category::Data => "unmatched data",
            Category::Eof => "unexpected eof",
        };
        Error::new(e).context(format!(
            "failed to deserialize .swcrc (json) file: {}: {}:{}",
            msg, line, column
        ))
    }

    let content = read_to_string(path).context("failed to read config (.swcrc) file")?;

    match serde_json::from_str(&content) {
        Ok(v) => return Ok(v),
        Err(..) => {}
    }

    serde_json::from_str::<Config>(&content)
        .map(Rc::Single)
        .map_err(convert_json_err)
}
