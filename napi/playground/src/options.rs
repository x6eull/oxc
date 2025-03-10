use napi_derive::napi;

#[napi(object)]
#[derive(Debug, Default, Clone)]
pub struct OxcOptions {
    pub run: Option<OxcRunOptions>,
    pub parser: Option<OxcParserOptions>,
    pub linter: Option<OxcLinterOptions>,
    pub transformer: Option<OxcTransformerOptions>,
    pub codegen: Option<OxcCodegenOptions>,
    pub minifier: Option<OxcMinifierOptions>,
    pub control_flow: Option<OxcControlFlowOptions>,
}

#[napi(object)]
#[derive(Debug, Default, Clone)]
pub struct OxcRunOptions {
    pub syntax: Option<bool>,
    pub lint: Option<bool>,
    pub format: Option<bool>,
    pub prettier_format: Option<bool>,
    pub prettier_ir: Option<bool>,
    pub transform: Option<bool>,
    pub type_check: Option<bool>,
    pub scope: Option<bool>,
    pub symbol: Option<bool>,
}

#[napi(object)]
#[derive(Debug, Default, Clone)]
pub struct OxcParserOptions {
    pub allow_return_outside_function: Option<bool>,
    pub preserve_parens: Option<bool>,
    pub allow_v8_intrinsics: Option<bool>,
    pub source_type: Option<String>,
    pub source_filename: Option<String>,
}

#[napi(object)]
#[derive(Debug, Default, Clone)]
// allow empty object for future compatibility
#[expect(clippy::empty_structs_with_brackets)]
pub struct OxcLinterOptions {}

#[napi(object)]
#[derive(Debug, Default, Clone)]
pub struct OxcTransformerOptions {
    pub target: Option<String>,
    pub isolated_declarations: Option<bool>,
}

#[napi(object)]
#[derive(Debug, Default, Clone)]
pub struct OxcCodegenOptions {
    pub indentation: Option<u8>,
    pub enable_typescript: Option<bool>,
    pub enable_sourcemap: Option<bool>,
}

#[napi(object)]
#[derive(Debug, Default, Clone)]
pub struct OxcControlFlowOptions {
    pub verbose: Option<bool>,
}

#[napi(object)]
#[derive(Debug, Default, Clone)]
pub struct OxcMinifierOptions {
    pub whitespace: Option<bool>,
    pub mangle: Option<bool>,
    pub compress: Option<bool>,
    pub compress_options: Option<OxcCompressOptions>,
}

#[napi(object)]
#[derive(Debug, Clone)]
pub struct OxcCompressOptions {
    pub booleans: bool,
    pub drop_debugger: bool,
    pub drop_console: bool,
    pub evaluate: bool,
    pub join_vars: bool,
    pub loops: bool,
    pub typeofs: bool,
}

// keep same with `oxc_minifier::options::CompressOptions`
impl Default for OxcCompressOptions {
    fn default() -> Self {
        Self {
            booleans: true,
            drop_debugger: true,
            drop_console: false,
            evaluate: true,
            join_vars: true,
            loops: true,
            typeofs: true,
        }
    }
}
