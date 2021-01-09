// Copyright 2020 The Evcxr Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;

use crate::{
    code_block::{CodeBlock, CodeKind, CommandCall, Segment},
    crash_guard::CrashGuard,
    errors::Span,
    eval_context::EvalCallbacks,
    rust_analyzer::{Completion, Completions},
    EvalContext, EvalContextOutputs, EvalOutputs,
};
use crate::{
    errors::{bail, CompilationError, Error},
    eval_context::ContextState,
};
use anyhow::Result;

/// A higher level interface to EvalContext. A bit closer to a Repl. Provides commands (start with
/// ':') that alter context state or print information.
pub struct CommandContext {
    print_timings: bool,
    eval_context: EvalContext,
    last_errors: Vec<CompilationError>,
}

impl CommandContext {
    pub fn new() -> Result<(CommandContext, EvalContextOutputs), Error> {
        let (eval_context, eval_context_outputs) = EvalContext::new()?;
        let command_context = CommandContext::with_eval_context(eval_context);
        Ok((command_context, eval_context_outputs))
    }

    pub fn with_eval_context(eval_context: EvalContext) -> CommandContext {
        CommandContext {
            print_timings: false,
            eval_context,
            last_errors: Vec::new(),
        }
    }

    #[doc(hidden)]
    pub fn new_for_testing() -> (CommandContext, EvalContextOutputs) {
        let (eval_context, outputs) = EvalContext::new_for_testing();
        (Self::with_eval_context(eval_context), outputs)
    }

    pub fn execute(&mut self, to_run: &str) -> Result<EvalOutputs, Error> {
        self.execute_with_callbacks(to_run, &mut EvalCallbacks::default())
    }

    pub fn check(&mut self, code: &str) -> Result<Vec<CompilationError>, Error> {
        let (user_code, nodes) = CodeBlock::from_original_user_code(code);
        let (non_command_code, state, errors) = self.prepare_for_analysis(user_code)?;
        if !errors.is_empty() {
            // If we've got errors while preparing, probably due to bad :dep commands, then there's
            // no point running cargo check as it'd just give us additional follow-on errors which
            // would be confusing.
            return Ok(errors);
        }
        self.eval_context.check(non_command_code, state, &nodes)
    }

    pub fn variables_and_types(&self) -> impl Iterator<Item = (&str, &str)> {
        self.eval_context.variables_and_types()
    }

    pub fn reset_config(&mut self) {
        self.eval_context.reset_config();
    }

    pub fn defined_item_names(&self) -> impl Iterator<Item = &str> {
        self.eval_context.defined_item_names()
    }

    pub fn execute_with_callbacks(
        &mut self,
        to_run: &str,
        callbacks: &mut EvalCallbacks,
    ) -> Result<EvalOutputs, Error> {
        let mut state = self.eval_context.state();
        state.clear_non_debug_relevant_fields();
        let mut guard = CrashGuard::new(|| {
            eprintln!(
                r#"
=============================================================================
Panic detected. Here's some useful information if you're filing a bug report.
<CODE>
{}
</CODE>
<STATE>
{:?}
</STATE>"#,
                to_run, state
            );
        });
        let result = self.execute_with_callbacks_internal(to_run, callbacks);
        guard.disarm();
        result
    }

    fn execute_with_callbacks_internal(
        &mut self,
        to_run: &str,
        callbacks: &mut EvalCallbacks,
    ) -> Result<EvalOutputs, Error> {
        use std::time::Instant;
        let mut eval_outputs = EvalOutputs::new();
        let start = Instant::now();
        let mut state = self.eval_context.state();
        let mut non_command_code = CodeBlock::new();
        let (user_code, nodes) = CodeBlock::from_original_user_code(to_run);
        for segment in user_code.segments {
            if let CodeKind::Command(command) = &segment.kind {
                eval_outputs.merge(self.execute_command(
                    command,
                    &segment,
                    &mut state,
                    &command.args,
                )?);
            } else {
                non_command_code = non_command_code.with_segment(segment);
            }
        }
        let result =
            self.eval_context
                .eval_with_callbacks(non_command_code, state, &nodes, callbacks);
        let duration = start.elapsed();
        match result {
            Ok(m) => {
                eval_outputs.merge(m);
                if self.print_timings {
                    eval_outputs.timing = Some(duration);
                }
                Ok(eval_outputs)
            }
            Err(Error::CompilationErrors(errors)) => {
                self.last_errors = errors.clone();
                Err(Error::CompilationErrors(errors))
            }
            x => x,
        }
    }

    pub fn set_opt_level(&mut self, level: &str) -> Result<(), Error> {
        self.eval_context.set_opt_level(level)
    }

    pub fn last_source(&self) -> std::io::Result<String> {
        self.eval_context.last_source()
    }

    /// Returns completions within `src` at `position`, which should be a byte offset. Note, this
    /// function requires &mut self because it mutates internal state in order to determine
    /// completions. It also assumes exclusive access to those resources. However there should be
    /// any visible side effects.
    pub fn completions(&mut self, src: &str, position: usize) -> Result<Completions> {
        let (user_code, nodes) = CodeBlock::from_original_user_code(src);
        if let Some((segment, offset)) = user_code.command_containing_user_offset(position) {
            return self.command_completions(segment, offset, position);
        }
        let (non_command_code, state, _errors) = self.prepare_for_analysis(user_code)?;
        self.eval_context
            .completions(non_command_code, state, &nodes, position)
    }

    fn prepare_for_analysis(
        &mut self,
        user_code: CodeBlock,
    ) -> Result<(CodeBlock, ContextState, Vec<CompilationError>)> {
        let mut non_command_code = CodeBlock::new();
        let mut state = self.eval_context.state();
        let mut errors = Vec::new();
        for segment in user_code.segments {
            if let CodeKind::Command(command) = &segment.kind {
                if let Err(error) =
                    self.process_command(command, &segment, &mut state, &command.args, true)
                {
                    errors.push(error);
                }
            } else {
                non_command_code = non_command_code.with_segment(segment);
            }
        }
        self.eval_context.write_cargo_toml(&state)?;
        Ok((non_command_code, state, errors))
    }

    fn command_completions(
        &self,
        segment: &Segment,
        offset: usize,
        full_position: usize,
    ) -> Result<Completions> {
        let existing = &segment.code[0..offset];
        let mut completions = Completions::default();
        completions.start_offset = full_position - offset;
        completions.end_offset = full_position;
        for cmd in Self::commands_by_name().keys() {
            if cmd.starts_with(existing) {
                completions.completions.push(Completion {
                    code: (*cmd).to_owned(),
                })
            }
        }
        Ok(completions)
    }

    fn load_config(&mut self) -> Result<EvalOutputs, Error> {
        let mut outputs = EvalOutputs::new();
        if let Some(config_dir) = crate::config_dir() {
            let config_file = config_dir.join("init.evcxr");
            if config_file.exists() {
                println!("Loading startup commands from {:?}", config_file);
                let contents = std::fs::read_to_string(config_file)?;
                for line in contents.lines() {
                    outputs.merge(self.execute(line)?);
                }
            }
            // Note: Loaded *after* init.evcxr so that it can access `:dep`s (or
            // any other state changed by :commands) specified in the init file.
            let prelude_file = config_dir.join("prelude.rs");
            if prelude_file.exists() {
                println!("Executing prelude from {:?}", prelude_file);
                let prelude = std::fs::read_to_string(prelude_file)?;
                outputs.merge(self.execute(&prelude)?);
            }
        }
        Ok(outputs)
    }

    fn execute_command(
        &mut self,
        command: &CommandCall,
        segment: &Segment,
        state: &mut ContextState,
        args: &Option<String>,
    ) -> Result<EvalOutputs, Error> {
        self.process_command(command, segment, state, args, false)
            .map_err(|err| Error::CompilationErrors(vec![err]))
    }

    fn process_command(
        &mut self,
        command_call: &CommandCall,
        segment: &Segment,
        state: &mut ContextState,
        args: &Option<String>,
        analysis_mode: bool,
    ) -> Result<EvalOutputs, CompilationError> {
        if let Some(command) = Self::commands_by_name().get(command_call.command.as_str()) {
            let result = match &command.analysis_callback {
                Some(analysis_callback) if analysis_mode => (analysis_callback)(self, state, args),
                _ => (command.callback)(self, state, args),
            };
            result.map_err(|error| {
                // Span from the start of the arguments to the end of the arguments, or if no
                // arguments are found, span the command. We look for the first non-space character
                // after a space is found.
                let mut found_space = false;
                let start_column = segment
                    .code
                    .chars()
                    .enumerate()
                    .find(|(_index, char)| {
                        if *char == ' ' {
                            found_space = true;
                            return false;
                        }
                        return found_space;
                    })
                    .map(|(index, _char)| index + 1)
                    .unwrap_or(1);
                let end_column = segment.code.chars().count();
                CompilationError::from_segment_span(
                    &segment,
                    error.to_string(),
                    Span::from_command(command_call, start_column, end_column),
                )
            })
        } else {
            return Err(CompilationError::from_segment_span(
                &segment,
                format!("Unrecognised command {}", command_call.command),
                Span::from_command(command_call, 1, command_call.command.chars().count() + 1),
            ));
        }
    }

    fn commands_by_name() -> &'static HashMap<&'static str, AvailableCommand> {
        lazy_static! {
            static ref COMMANDS_BY_NAME: HashMap<&'static str, AvailableCommand> =
                CommandContext::create_commands()
                    .into_iter()
                    .map(|command| (command.name, command))
                    .collect();
        }
        &COMMANDS_BY_NAME
    }

    fn create_commands() -> Vec<AvailableCommand> {
        vec![
            AvailableCommand::new(
                ":internal_debug",
                "Toggle various internal debugging code",
                |_ctx, state, _args| {
                    let debug_mode = !state.debug_mode();
                    state.set_debug_mode(debug_mode);
                    text_output(format!("Internals debugging: {}", debug_mode))
                },
            ),
            AvailableCommand::new(
                ":load_config",
                "Reloads startup configuration files",
                |ctx, state, _args| {
                    let result = ctx.load_config();
                    *state = ctx.eval_context.state();
                    result
                },
            )
            .disable_in_analysis(),
            AvailableCommand::new(":version", "Print Evcxr version", |_ctx, _state, _args| {
                text_output(env!("CARGO_PKG_VERSION"))
            }),
            AvailableCommand::new(
                ":vars",
                "List bound variables and their types",
                |ctx, _state, _args| {
                    Ok(EvalOutputs::text_html(
                        ctx.vars_as_text(),
                        ctx.vars_as_html(),
                    ))
                },
            ),
            AvailableCommand::new(
                ":preserve_vars_on_panic",
                "Try to keep vars on panic (0/1)",
                |_ctx, state, args| {
                    state
                        .set_preserve_vars_on_panic(args.as_ref().map(String::as_str) == Some("1"));
                    text_output(format!(
                        "Preserve vars on panic: {}",
                        state.preserve_vars_on_panic()
                    ))
                },
            ),
            AvailableCommand::new(
                ":clear",
                "Clear all state, keeping compilation cache",
                |ctx, state, _args| {
                    ctx.eval_context.clear().map(|_| {
                        *state = ctx.eval_context.state();
                        EvalOutputs::new()
                    })
                },
            )
            .with_analysis_callback(|ctx, state, _args| {
                *state = ctx.eval_context.cleared_state();
                Ok(EvalOutputs::default())
            }),
            AvailableCommand::new(
                ":dep",
                "Add dependency. e.g. :dep regex = \"1.0\"",
                |_ctx, state, args| process_dep_command(state, args),
            ),
            AvailableCommand::new(
                ":last_compile_dir",
                "Print the directory in which we last compiled",
                |ctx, _state, _args| {
                    text_output(format!("{:?}", ctx.eval_context.last_compile_dir()))
                },
            ),
            AvailableCommand::new(
                ":opt",
                "Set optimization level (0/1/2)",
                |_ctx, state, args| {
                    let new_level = if let Some(n) = args {
                        &n
                    } else if state.opt_level() == "2" {
                        "0"
                    } else {
                        "2"
                    };
                    state.set_opt_level(new_level)?;
                    text_output(format!("Optimization: {}", state.opt_level()))
                },
            ),
            AvailableCommand::new(
                ":fmt",
                "Set output formatter (default: {:?})",
                |_ctx, state, args| {
                    let new_format = if let Some(f) = args { f } else { "{:?}" };
                    state.set_output_format(new_format.to_owned());
                    text_output(format!("Output format: {}", state.output_format()))
                },
            ),
            AvailableCommand::new(
                ":efmt",
                "Set the formatter for errors returned by ?",
                |_ctx, state, args| {
                    if let Some(f) = args {
                        state.set_error_format(f)?;
                    }
                    text_output(format!(
                        "Error format: {} (errors must implement {})",
                        state.error_format(),
                        state.error_format_trait()
                    ))
                },
            ),
            AvailableCommand::new(
                ":toolchain",
                "Set which toolchain to use (e.g. nightly)",
                |_ctx, state, args| {
                    if let Some(arg) = args {
                        state.set_toolchain(&arg);
                    }
                    text_output(format!("Toolchain: {}", state.toolchain()))
                },
            ),
            AvailableCommand::new(
                ":offline",
                "Set offline mode when invoking cargo",
                |_ctx, state, args| {
                    state.set_offline_mode(args.as_ref().map(String::as_str) == Some("1"));
                    text_output(format!("Offline mode: {}", state.offline_mode()))
                },
            ),
            AvailableCommand::new(
                ":quit",
                "Quit evaluation and exit",
                |_ctx, _state, _args| std::process::exit(0),
            )
            .disable_in_analysis(),
            AvailableCommand::new(
                ":timing",
                "Toggle printing of how long evaluations take",
                |ctx, _state, _args| {
                    ctx.print_timings = !ctx.print_timings;
                    text_output(format!("Timing: {}", ctx.print_timings))
                },
            ),
            AvailableCommand::new(
                ":time_passes",
                "Toggle printing of rustc pass times (requires nightly)",
                |_ctx, state, _args| {
                    state.set_time_passes(!state.time_passes());
                    text_output(format!("Time passes: {}", state.time_passes()))
                },
            ),
            AvailableCommand::new(
                ":sccache",
                "Set whether to use sccache (0/1).",
                |_ctx, state, args| {
                    state.set_sccache(args.as_ref().map(String::as_str) != Some("0"))?;
                    text_output(format!("sccache: {}", state.sccache()))
                },
            ),
            AvailableCommand::new(
                ":linker",
                "Set/print linker. Supported: system, lld",
                |_ctx, state, args| {
                    if let Some(linker) = args {
                        state.set_linker(linker.to_owned());
                    }
                    text_output(format!("linker: {}", state.linker()))
                },
            ),
            AvailableCommand::new(
                ":explain",
                "Print explanation of last error",
                |ctx, _state, _args| {
                    if ctx.last_errors.is_empty() {
                        bail!("No last error to explain");
                    } else {
                        let mut all_explanations = String::new();
                        for error in &ctx.last_errors {
                            if let Some(explanation) = error.explanation() {
                                all_explanations.push_str(explanation);
                            } else {
                                bail!("Sorry, last error has no explanation");
                            }
                        }
                        text_output(all_explanations)
                    }
                },
            ),
            AvailableCommand::new(
                ":last_error_json",
                "Print the last compilation error as JSON (for debugging)",
                |ctx, _state, _args| {
                    let mut errors_out = String::new();
                    for error in &ctx.last_errors {
                        use std::fmt::Write;
                        write!(&mut errors_out, "{}", error.json)?;
                        errors_out.push('\n');
                    }
                    bail!(errors_out);
                },
            ),
            AvailableCommand::new(":help", "Print command help", |_ctx, _state, _args| {
                use std::fmt::Write;
                let mut text = String::new();
                let mut html = String::new();
                writeln!(&mut html, "<table>")?;
                let mut commands = CommandContext::create_commands();
                commands.sort_by(|a, b| a.name.cmp(b.name));
                for cmd in commands {
                    writeln!(&mut text, "{:<17} {}", cmd.name, cmd.short_description).unwrap();
                    writeln!(
                        &mut html,
                        "<tr><td>{}</td><td>{}</td></tr>",
                        cmd.name, cmd.short_description
                    )?;
                }
                writeln!(&mut html, "</table>")?;
                Ok(EvalOutputs::text_html(text, html))
            }),
        ]
    }

    fn vars_as_text(&self) -> String {
        let mut out = String::new();
        for (var, ty) in self.eval_context.variables_and_types() {
            out.push_str(var);
            out.push_str(": ");
            out.push_str(ty);
            out.push_str("\n");
        }
        out
    }

    fn vars_as_html(&self) -> String {
        let mut out = String::new();
        out.push_str("<table><tr><th>Variable</th><th>Type</th></tr>");
        for (var, ty) in self.eval_context.variables_and_types() {
            out.push_str("<tr><td>");
            html_escape(var, &mut out);
            out.push_str("</td><td>");
            html_escape(ty, &mut out);
            out.push_str("</td><tr>");
        }
        out.push_str("</table>");
        out
    }
}

fn process_dep_command(
    state: &mut ContextState,
    args: &Option<String>,
) -> Result<EvalOutputs, Error> {
    use regex::Regex;
    let args = if let Some(v) = args {
        v
    } else {
        bail!(":dep requires arguments")
    };
    lazy_static! {
        static ref DEP_RE: Regex = Regex::new("^([^= ]+) *(= *(.+))?$").unwrap();
    }
    if let Some(captures) = DEP_RE.captures(args) {
        state.add_dep(
            &captures[1],
            &captures.get(3).map_or("\"*\"", |m| m.as_str()),
        )?;
        Ok(EvalOutputs::new())
    } else {
        bail!("Invalid :dep command. Expected: name = ... or just name");
    }
}

struct AvailableCommand {
    name: &'static str,
    short_description: &'static str,
    callback: Box<
        dyn Fn(
                &mut CommandContext,
                &mut ContextState,
                &Option<String>,
            ) -> Result<EvalOutputs, Error>
            + 'static
            + Sync,
    >,
    /// If `Some`, this callback will be run when preparing for analysis instead of `callback`.
    analysis_callback: Option<
        Box<
            dyn Fn(
                    &CommandContext,
                    &mut ContextState,
                    &Option<String>,
                ) -> Result<EvalOutputs, Error>
                + 'static
                + Sync,
        >,
    >,
}

impl AvailableCommand {
    fn new(
        name: &'static str,
        short_description: &'static str,
        callback: impl Fn(
                &mut CommandContext,
                &mut ContextState,
                &Option<String>,
            ) -> Result<EvalOutputs, Error>
            + 'static
            + Sync,
    ) -> AvailableCommand {
        AvailableCommand {
            name,
            short_description,
            callback: Box::new(callback),
            analysis_callback: None,
        }
    }

    fn with_analysis_callback(
        mut self,
        callback: impl Fn(&CommandContext, &mut ContextState, &Option<String>) -> Result<EvalOutputs, Error>
            + 'static
            + Sync,
    ) -> Self {
        self.analysis_callback = Some(Box::new(callback));
        self
    }

    fn disable_in_analysis(self) -> Self {
        self.with_analysis_callback(|_ctx, _state, _args| Ok(EvalOutputs::default()))
    }
}

fn html_escape(input: &str, out: &mut String) {
    for ch in input.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            x => out.push(x),
        }
    }
}

fn text_output<T: Into<String>>(text: T) -> Result<EvalOutputs, Error> {
    let mut outputs = EvalOutputs::new();
    let mut content = text.into();
    content.push('\n');
    outputs
        .content_by_mime_type
        .insert("text/plain".to_owned(), content);
    Ok(outputs)
}
