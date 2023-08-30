use std::{collections::HashMap, io::Write, path::Path, rc::Rc};

use crate::{
    data::{ResourceLocation, ScoreboardEntry},
    parser::{FunctionDefinition, JumpInfo, Operation, ParseError, Parser, ParserNode},
    backend::type_pool::TypePool,
    backend::types::SculkType,
};

use super::{
    function::{Function, FunctionSignature, ParamDef},
    rebranch,
    validate::{ValidationError, Validator},
};

pub struct CodeGenerator {
    unfinished_functions: Vec<Function>,
    ready_functions: HashMap<String, Function>,
    func_signatures: HashMap<String, FunctionSignature>,
    type_pool: TypePool,
    eval_stacks: Vec<EvaluationStack>,
    bin_op_depth: i32,
    anon_func_depth: i32,
    flag_tmp_count: i32,
    loop_depth: i32,
    propagate_return: bool,
    propagate_break: bool,
    namespace: String,
}

impl CodeGenerator {
    pub fn compile_src(src: &str, namespace: &str) -> Result<Self, Vec<CompileError>> {
        let parser = Parser::new(src);
        let mut errors = Vec::new();

        let mut parse_output = parser.parse();

        let validator = Validator::new();

        let (func_signatures, type_pool, validation_errs) = validator.dissolve();

        errors.extend(
            parse_output
                .errs
                .into_iter()
                .map(|err| CompileError::Parse(err)),
        );

        errors.extend(
            validation_errs
                .into_iter()
                .map(|err| CompileError::Validate(err)),
        );

        if errors.len() > 0 {
            return Err(errors);
        }

        rebranch::rebranch(&mut parse_output.ast);

        let mut sculk_main = Function::new_empty(
            "_sculkmain".to_string(),
            ResourceLocation::new(namespace.to_string(), "_sculkmain".to_string()),
            vec![],
            type_pool.none(),
        );

        let mut gen = Self {
            unfinished_functions: vec![],
            ready_functions: HashMap::new(),
            func_signatures,
            type_pool,
            eval_stacks: vec![],
            bin_op_depth: 0,
            anon_func_depth: 0,
            flag_tmp_count: 0,
            loop_depth: 0,
            propagate_return: false,
            propagate_break: false,
            namespace: namespace.to_string(),
        };

        gen.compile(&parse_output.ast);

        for func in gen
            .ready_functions
            .values()
            .filter(|func| !func.is_anonymous())
        {
            sculk_main.actions.push(Action::CreateStorage {
                name: ResourceLocation::scoreboard(namespace.to_string(), func.name().to_string())
                    .to_string(),
            });
        }

        sculk_main.actions.push(Action::CallFunction {
            target: ResourceLocation::new(namespace.to_string(), "main".to_string()),
        });

        gen.ready_functions
            .insert("_sculkmain".to_string(), sculk_main);

        Ok(gen)
    }

    // TODO: no more unwraps here
    pub fn output_to_dir(&self, dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let mut output = String::new();

        for (_, func) in &self.ready_functions {
            for action in &func.actions {
                Self::write_action(&mut output, &action);
                output.push_str("\r\n");
            }

            let mut path = dir.to_path_buf();
            path.push(&func.name());
            path.set_extension("mcfunction");

            let mut file = std::fs::File::create(path).unwrap();
            file.write(output.as_bytes()).unwrap();

            output.clear();
        }
    }

    fn compile(&mut self, ast: &ParserNode) {
        self.visit_node(ast);
    }

    fn visit_node(&mut self, node: &ParserNode) {
        match node {
            ParserNode::NumberLiteral(num) => self.visit_number(*num),
            ParserNode::BoolLiteral(bool) => self.visit_bool(*bool),
            ParserNode::Identifier(name) => self.visit_identifier(name),
            ParserNode::Operation(lhs, rhs, op) => self.visit_binary_operation(lhs, rhs, *op),
            ParserNode::OpEquals { name, expr, op } => self.visit_op_equals(name, expr, *op),
            ParserNode::VariableDeclaration { name, expr, ty: _ } => {
                self.visit_variable_assignment(name, expr)
            }
            ParserNode::VariableAssignment { name, expr } => {
                self.visit_variable_assignment(name, expr)
            }
            ParserNode::Program(nodes) => self.visit_program(nodes),
            ParserNode::FunctionDeclaration {
                name, args, body, ..
            } => self.visit_function_declaration(name, args, body),
            ParserNode::FunctionCall { name, args } => self.visit_function_call(name, args),
            ParserNode::Return(expr) => self.visit_return(expr),
            ParserNode::Block(nodes) => self.visit_block(nodes),
            ParserNode::TypedIdentifier { .. } => {}
            ParserNode::Unary(expr, op) => self.visit_unary(expr, *op),
            ParserNode::If {
                cond,
                body,
                else_ifs,
                else_body,
            } => self.visit_if(cond, body, else_ifs, else_body),
            ParserNode::For {
                init,
                cond,
                step,
                body,
            } => {
                self.visit_for(init, cond, step, body);
            }
            ParserNode::Break => self.visit_break(),
            ParserNode::CommandLiteral(command) => self.visit_command_literal(command),
            ParserNode::StructDefinition { .. } => {} // nothing to be done, structs are handled in the validator
        }

        self.propagate_return = false;
    }

    fn visit_program(&mut self, nodes: &[ParserNode]) {
        for node in nodes {
            self.visit_node(node);
        }
    }

    fn visit_block(&mut self, nodes: &[ParserNode]) {
        for node in nodes {
            self.visit_node(node);
        }
    }

    fn visit_number(&mut self, num: i32) {
        self.push_eval_instr(EvaluationInstruction::PushNumber(num));
    }

    fn visit_bool(&mut self, bool: bool) {
        self.push_eval_instr(EvaluationInstruction::PushBool(bool));
    }

    // TODO: implement shadowing
    fn visit_identifier(&mut self, name: &str) {
        self.push_eval_instr(EvaluationInstruction::PushVariable(
            self.local_variable(name),
        ));
    }

    fn visit_binary_operation(&mut self, lhs: &ParserNode, rhs: &ParserNode, op: Operation) {
        self.bin_op_depth += 1;

        self.visit_node(lhs);
        self.visit_node(rhs);
        self.push_eval_instr(EvaluationInstruction::Operation(op));

        self.bin_op_depth -= 1;
    }

    fn visit_op_equals(&mut self, name: &str, expr: &ParserNode, op: Operation) {
        self.begin_evaluation_for_scoreboard(self.active_scoreboard(), 0);
        self.visit_node(expr);
        let result_tmp = self.end_current_evaluation();

        match op {
            Operation::Add => {
                self.emit_action(Action::AddVariables {
                    first: self.local_variable(name),
                    second: self.get_tmp(result_tmp),
                });
            }
            Operation::Subtract => {
                self.emit_action(Action::SubtractVariables {
                    first: self.local_variable(name),
                    second: self.get_tmp(result_tmp),
                });
            }
            Operation::Multiply => {
                self.emit_action(Action::MultiplyVariables {
                    first: self.local_variable(name),
                    second: self.get_tmp(result_tmp),
                });
            }
            Operation::Divide => {
                self.emit_action(Action::DivideVariables {
                    first: self.local_variable(name),
                    second: self.get_tmp(result_tmp),
                });
            }
            Operation::Modulo => {
                self.emit_action(Action::ModuloVariables {
                    first: self.local_variable(name),
                    second: self.get_tmp(result_tmp),
                });
            }
            _ => unreachable!(),
        }
    }

    fn visit_unary(&mut self, expr: &ParserNode, op: Operation) {
        match op {
            Operation::Negate => {
                self.visit_node(expr);
                self.push_eval_instr(EvaluationInstruction::PushNumber(-1));
                self.push_eval_instr(EvaluationInstruction::Operation(Operation::Multiply));
            }
            Operation::Not => {
                self.push_eval_instr(EvaluationInstruction::PushNumber(1));
                self.visit_node(expr);
                self.push_eval_instr(EvaluationInstruction::Operation(Operation::Subtract));
            }
            _ => unreachable!(),
        }
    }

    fn visit_variable_assignment(&mut self, name: &str, val: &ParserNode) {
        self.begin_evaluation_for_scoreboard(self.active_scoreboard(), 0);
        self.visit_node(val);
        let result_tmp = self.end_current_evaluation();
        let var = self.local_variable(name);
        self.emit_action(Action::SetVariableToVariable {
            first: var,
            second: self.get_tmp(result_tmp),
        });
    }

    // TODO: stop people from defining functions within other functions?
    fn visit_function_declaration(&mut self, name: &str, args: &[ParserNode], body: &ParserNode) {
        let func_signature = self.func_signatures.get(name).unwrap();

        self.unfinished_functions.push(Function::new_empty(
            name.to_string(),
            self.scoreboard(name),
            func_signature.params().to_vec(),
            func_signature.return_type(),
        ));

        if func_signature.return_type() != self.type_pool.none() {
            self.emit_action(Action::SetVariableToNumber {
                var: self.local_variable("RETFLAG"),
                val: 0,
            });
        }

        self.visit_node(body);

        self.ready_functions
            .insert(name.to_string(), self.unfinished_functions.pop().unwrap());

        self.flag_tmp_count = 0;
    }

    fn visit_function_call(&mut self, name: &str, args: &[ParserNode]) {
        let use_new_stack = self.eval_stacks.is_empty();

        if use_new_stack {
            self.begin_evaluation_for_scoreboard(self.active_scoreboard(), 0);
        }

        for arg in args.iter() {
            self.visit_node(arg);
        }

        self.push_eval_instr(EvaluationInstruction::CallFunction(
            self.resource_location(name),
            self.func_signatures[name]
                .params()
                .iter()
                .map(|p| p.name().to_string())
                .collect(),
        ));

        if use_new_stack {
            self.end_current_evaluation();
        }
    }

    fn visit_return(&mut self, expr: &Option<Box<ParserNode>>) {
        if let Some(expr) = expr {
            self.begin_evaluation_for_scoreboard(self.active_scoreboard(), 0);
            self.visit_node(expr);
            let result_tmp = self.end_current_evaluation();

            self.emit_action(Action::SetVariableToVariable {
                first: self.local_variable("RET"),
                second: self.get_tmp(result_tmp),
            });
        }

        self.emit_action(Action::SetVariableToNumber {
            var: self.local_variable("RETFLAG"),
            val: 1,
        });

        self.emit_action(Action::Return);

        self.propagate_return = true;
    }

    fn visit_jump_safe(&mut self, node: &ParserNode, jump_info: JumpInfo) {
        let not_ret_func = self.current_function().make_anonymous_child();
        let not_ret_func_name = not_ret_func.name().to_string();

        self.unfinished_functions.push(not_ret_func);
        self.visit_node(node);
        self.ready_functions.insert(
            not_ret_func_name.clone(),
            self.unfinished_functions.pop().unwrap(),
        );

        if jump_info.may_return {
            self.emit_action(Action::ExecuteIf {
                condition: format!("score {} matches 1", self.local_variable("RETFLAG")),
                then: Box::new(Action::Return),
            });
        }

        if jump_info.may_break {
            self.emit_action(Action::ExecuteIf {
                condition: format!("score {} matches 1", self.current_break_flag()),
                then: Box::new(Action::Return),
            });
        }
    }

    fn visit_command_literal(&mut self, command: &str) {
        self.emit_action(Action::Direct {
            command: command.to_string(),
        });
    }

    fn visit_if(
        &mut self,
        cond: &ParserNode,
        body: &ParserNode,
        _else_ifs: &[(ParserNode, ParserNode)], // at this stage, there are no else-ifs, they have been converted to nested ifs
        else_body: &Option<Box<ParserNode>>,
    ) {
        self.begin_evaluation_for_scoreboard(self.active_scoreboard(), 0);
        self.visit_node(cond);
        let result_tmp = self.end_current_evaluation();
        let flag_tmp = self.flag_tmp_count;
        self.flag_tmp_count += 1;

        let true_func = self.current_function().make_anonymous_child();
        let true_func_name = true_func.name().to_string();

        self.unfinished_functions.push(true_func);
        self.visit_node(body);
        self.ready_functions.insert(
            true_func_name.clone(),
            self.unfinished_functions.pop().unwrap(),
        );

        self.emit_action(Action::ExecuteIf {
            condition: format!("score {} matches 1", self.get_tmp(result_tmp)),
            then: Box::new(Action::CallFunction {
                target: self.resource_location(&true_func_name),
            }),
        });

        self.account_for_jumps();

        if let Some(else_body) = else_body {
            let false_func = self.current_function().make_anonymous_child();
            let false_func_name = false_func.name().to_string();

            self.unfinished_functions.push(false_func);
            dbg!(&else_body);
            self.visit_node(else_body);
            self.ready_functions.insert(
                false_func_name.to_string(),
                self.unfinished_functions.pop().unwrap(),
            );

            self.emit_action(Action::ExecuteUnless {
                condition: format!("score {} matches 1", self.get_tmp(result_tmp)),
                then: Box::new(Action::CallFunction {
                    target: self.resource_location(&false_func_name),
                }),
            });
        }

        self.account_for_jumps();
    }

    fn visit_for(
        &mut self,
        init: &ParserNode,
        cond: &ParserNode,
        step: &ParserNode,
        body: &ParserNode,
    ) {
        self.loop_depth += 1;

        self.visit_node(init);

        let loop_func = self.current_function().make_anonymous_child();
        let loop_func_name = loop_func.name().to_string();

        self.unfinished_functions.push(loop_func);
        self.visit_node(body);
        self.visit_node(step);

        self.begin_evaluation_for_scoreboard(self.active_scoreboard(), 0);
        self.visit_node(cond);
        let flag_tmp = self.end_current_evaluation();

        self.emit_action(Action::ExecuteIf {
            condition: format!("score {} matches 1", self.get_tmp(flag_tmp)),
            then: Box::new(Action::CallFunction {
                target: self.resource_location(&loop_func_name),
            }),
        });

        self.ready_functions.insert(
            loop_func_name.clone(),
            self.unfinished_functions.pop().unwrap(),
        );

        self.begin_evaluation_for_scoreboard(self.active_scoreboard(), 0);
        self.visit_node(cond);
        let flag_tmp = self.end_current_evaluation();
        self.flag_tmp_count += 1;

        self.emit_action(Action::ExecuteIf {
            condition: format!("score {} matches 1", self.get_tmp(flag_tmp)),
            then: Box::new(Action::CallFunction {
                target: self.resource_location(&loop_func_name),
            }),
        });

        self.propagate_break = false;
        self.account_for_jumps();

        self.loop_depth -= 1;
    }

    fn visit_break(&mut self) {
        self.emit_action(Action::SetVariableToNumber {
            var: self.current_break_flag(),
            val: 1,
        });

        self.emit_action(Action::Return);

        self.propagate_break = true;
    }

    fn emit_action(&mut self, action: Action) {
        self.current_function_mut().actions.push(action);
    }

    fn current_function(&self) -> &Function {
        self.unfinished_functions.last().unwrap()
    }

    fn current_function_mut(&mut self) -> &mut Function {
        self.unfinished_functions.last_mut().unwrap()
    }

    fn push_eval_instr(&mut self, instr: EvaluationInstruction) {
        self.eval_stacks.last_mut().unwrap().push_instruction(instr);
    }

    fn get_tmp(&self, num: i32) -> ScoreboardEntry {
        self.local_variable(&format!("TMP{}", num))
    }

    fn get_flag(&self, num: i32) -> ScoreboardEntry {
        self.local_variable(&format!("FLAG{}", num))
    }

    fn active_scoreboard(&self) -> ResourceLocation {
        self.current_function().scoreboard().clone()
    }

    fn begin_evaluation_for_scoreboard(&mut self, scoreboard: ResourceLocation, min_tmp: i32) {
        self.eval_stacks
            .push(EvaluationStack::new(scoreboard, min_tmp));
    }

    fn end_current_evaluation(&mut self) -> i32 {
        let mut last_stack = self.eval_stacks.pop().unwrap();
        let result_tmp = last_stack.flush();

        let actions = last_stack.actions;

        for action in actions {
            self.emit_action(action);
        }

        result_tmp
    }

    fn write_action(str: &mut String, action: &Action) {
        match action {
            Action::CreateStorage { name } => {
                str.push_str(&format!("scoreboard objectives add {} dummy", name))
            }
            Action::SetVariableToNumber { var: name, val } => {
                str.push_str(&format!("scoreboard players set {} {}", name, val))
            }
            Action::AddVariables { first, second } => str.push_str(&format!(
                "scoreboard players operation {} += {}",
                first, second
            )),
            Action::SubtractVariables { first, second } => str.push_str(&format!(
                "scoreboard players operation {} -= {}",
                first, second
            )),
            Action::MultiplyVariables { first, second } => str.push_str(&format!(
                "scoreboard players operation {} *= {}",
                first, second
            )),
            Action::DivideVariables { first, second } => str.push_str(&format!(
                "scoreboard players operation {} /= {}",
                first, second
            )),
            Action::ModuloVariables { first, second } => str.push_str(&format!(
                "scoreboard players operation {} %= {}",
                first, second
            )),
            Action::SetVariableToVariable { first, second } => str.push_str(&format!(
                "scoreboard players operation {} = {}",
                first, second
            )),
            Action::CallFunction { target } => str.push_str(&format!("function {}", target)),
            Action::ExecuteIf { condition, then } => {
                str.push_str(&format!("execute if {} run ", condition));
                Self::write_action(str, then);
            }
            Action::ExecuteUnless { condition, then } => {
                str.push_str(&format!("execute unless {} run ", condition));
                Self::write_action(str, then);
            }
            Action::Direct { command } => str.push_str(command),
            Action::Return => str.push_str("return"),
        }
    }

    fn resource_location(&self, path: &str) -> ResourceLocation {
        ResourceLocation::new(self.namespace.clone(), path.to_string())
    }

    fn scoreboard(&self, name: &str) -> ResourceLocation {
        ResourceLocation::scoreboard(self.namespace.clone(), name.to_string())
    }

    fn local_variable(&self, name: &str) -> ScoreboardEntry {
        ScoreboardEntry::new(
            self.current_function().scoreboard().clone(),
            name.to_string(),
        )
    }

    fn current_break_flag(&self) -> ScoreboardEntry {
        self.local_variable(&format!("BREAKFLAG{}", self.loop_depth))
    }

    fn account_for_return(&mut self) {
        if !self.propagate_return {
            return;
        }

        self.emit_action(Action::ExecuteIf {
            condition: format!("score {} matches 1", self.local_variable("RETFLAG")),
            then: Box::new(Action::Return),
        });
    }

    fn account_for_break(&mut self) {
        if !self.propagate_break {
            return;
        }

        self.emit_action(Action::ExecuteIf {
            condition: format!("score {} matches 1", self.current_break_flag()),
            then: Box::new(Action::Return),
        });
    }

    fn account_for_jumps(&mut self) {
        self.account_for_return();
        self.account_for_break();
    }
}

#[derive(Debug)]
pub enum Action {
    CreateStorage {
        name: String,
    },
    SetVariableToNumber {
        var: ScoreboardEntry,
        val: i32,
    },
    AddVariables {
        first: ScoreboardEntry,
        second: ScoreboardEntry,
    },
    SubtractVariables {
        first: ScoreboardEntry,
        second: ScoreboardEntry,
    },
    MultiplyVariables {
        first: ScoreboardEntry,
        second: ScoreboardEntry,
    },
    DivideVariables {
        first: ScoreboardEntry,
        second: ScoreboardEntry,
    },
    ModuloVariables {
        first: ScoreboardEntry,
        second: ScoreboardEntry,
    },
    SetVariableToVariable {
        first: ScoreboardEntry,
        second: ScoreboardEntry,
    },
    CallFunction {
        target: ResourceLocation,
    },
    ExecuteIf {
        condition: String,
        then: Box<Action>,
    },
    ExecuteUnless {
        condition: String,
        then: Box<Action>,
    },
    Direct {
        command: String,
    },
    Return,
}

#[derive(Debug)]
pub enum CompileError {
    Parse(ParseError),
    Validate(ValidationError),
    InvalidTypes,
}

impl CompileError {
    fn parse_error(error: ParseError) -> Self {
        CompileError::Parse(error)
    }
}

#[derive(Debug, Clone)]
enum EvaluationInstruction {
    PushNumber(i32),
    PushBool(bool),
    PushVariable(ScoreboardEntry),
    Operation(Operation),
    CallFunction(ResourceLocation, Vec<String>),
}

impl EvaluationInstruction {
    fn as_operation(self) -> Operation {
        match self {
            EvaluationInstruction::Operation(op) => op,
            _ => panic!("Cannot convert non-operation to operation"),
        }
    }
}

struct EvaluationStack {
    instructions: Vec<EvaluationInstruction>,
    actions: Vec<Action>,
    available_tmps: Vec<i32>,
    max_tmps: i32,
    scoreboard: ResourceLocation,
}

impl EvaluationStack {
    fn new(scoreboard: ResourceLocation, min_tmp: i32) -> Self {
        EvaluationStack {
            instructions: Vec::new(),
            actions: Vec::new(),
            available_tmps: Vec::new(),
            max_tmps: min_tmp,
            scoreboard,
        }
    }

    fn push_instruction(&mut self, instr: EvaluationInstruction) {
        self.instructions.push(instr);
    }

    fn flush(&mut self) -> i32 {
        // keep track of tmps that were used for intermediate operations
        // we need to free them after the full operation is done
        let mut intermediate_tmps = Vec::new();

        for i in 0..self.instructions.len() {
            let instr = self.instructions[i].clone(); // i am so mad

            match instr {
                EvaluationInstruction::PushNumber(num) => {
                    let tmp_idx = self.reserve_available_tmp();
                    let tmp_var = self.get_tmp(tmp_idx);
                    self.emit_action(Action::SetVariableToNumber {
                        var: tmp_var,
                        val: num,
                    });
                    intermediate_tmps.push(tmp_idx);
                }
                EvaluationInstruction::PushBool(bool) => {
                    let tmp_idx = self.reserve_available_tmp();
                    let tmp_var = self.get_tmp(tmp_idx);
                    let bool_val = if bool { 1 } else { 0 };
                    self.emit_action(Action::SetVariableToNumber {
                        var: tmp_var,
                        val: bool_val,
                    });
                    intermediate_tmps.push(tmp_idx);
                }
                EvaluationInstruction::PushVariable(name) => {
                    let tmp_idx = self.reserve_available_tmp();
                    let tmp_var = self.get_tmp(tmp_idx);
                    self.emit_action(Action::SetVariableToVariable {
                        first: tmp_var,
                        second: name,
                    });
                    intermediate_tmps.push(tmp_idx);
                }
                EvaluationInstruction::Operation(op) => {
                    let tmp_b_idx = intermediate_tmps.pop().unwrap();
                    let tmp_a_idx = *intermediate_tmps.last().unwrap();

                    let tmp_a_var = self.get_tmp(tmp_a_idx);
                    let tmp_b_var = self.get_tmp(tmp_b_idx);

                    match op {
                        Operation::Add => self.emit_action(Action::AddVariables {
                            first: tmp_a_var,
                            second: tmp_b_var,
                        }),
                        Operation::Subtract => self.emit_action(Action::SubtractVariables {
                            first: tmp_a_var,
                            second: tmp_b_var,
                        }),
                        Operation::Multiply => self.emit_action(Action::MultiplyVariables {
                            first: tmp_a_var,
                            second: tmp_b_var,
                        }),
                        Operation::Divide => self.emit_action(Action::DivideVariables {
                            first: tmp_a_var,
                            second: tmp_b_var,
                        }),
                        Operation::Modulo => self.emit_action(Action::ModuloVariables {
                            first: tmp_a_var,
                            second: tmp_b_var,
                        }),
                        Operation::GreaterThan => {
                            self.emit_action(Action::SubtractVariables {
                                first: tmp_a_var.clone(),
                                second: tmp_b_var.clone(),
                            });
                            self.emit_action(Action::ExecuteIf {
                                condition: format!("score {} matches 1..", &tmp_a_var),
                                then: Box::new(Action::SetVariableToNumber {
                                    var: tmp_a_var,
                                    val: 1,
                                }),
                            });
                        }
                        Operation::LessThan => {
                            self.emit_action(Action::SubtractVariables {
                                first: tmp_a_var.clone(),
                                second: tmp_b_var.clone(),
                            });
                            self.emit_action(Action::ExecuteIf {
                                condition: format!("score {} matches ..-1", &tmp_a_var),
                                then: Box::new(Action::SetVariableToNumber {
                                    var: tmp_a_var,
                                    val: 1,
                                }),
                            });
                        }
                        Operation::GreaterThanOrEquals => {
                            self.emit_action(Action::SubtractVariables {
                                first: tmp_a_var.clone(),
                                second: tmp_b_var.clone(),
                            });
                            self.emit_action(Action::ExecuteIf {
                                condition: format!("score {} matches 0..", &tmp_a_var),
                                then: Box::new(Action::SetVariableToNumber {
                                    var: tmp_a_var,
                                    val: 1,
                                }),
                            });
                        }
                        Operation::LessThanOrEquals => {
                            self.emit_action(Action::SubtractVariables {
                                first: tmp_a_var.clone(),
                                second: tmp_b_var.clone(),
                            });
                            self.emit_action(Action::ExecuteIf {
                                condition: format!("score {} matches ..0", &tmp_a_var),
                                then: Box::new(Action::SetVariableToNumber {
                                    var: tmp_a_var,
                                    val: 1,
                                }),
                            });
                        }
                        Operation::CheckEquals => {
                            self.emit_action(Action::SubtractVariables {
                                first: tmp_a_var.clone(),
                                second: tmp_b_var.clone(),
                            });
                            self.emit_action(Action::ExecuteIf {
                                condition: format!("score {} matches 0", &tmp_a_var),
                                then: Box::new(Action::SetVariableToNumber {
                                    var: tmp_a_var,
                                    val: 1,
                                }),
                            });
                        }
                        Operation::NotEquals => {
                            self.emit_action(Action::SubtractVariables {
                                first: tmp_a_var.clone(),
                                second: tmp_b_var.clone(),
                            });
                            self.emit_action(Action::ExecuteUnless {
                                condition: format!("score {} matches 0", &tmp_a_var),
                                then: Box::new(Action::SetVariableToNumber {
                                    var: tmp_a_var,
                                    val: 1,
                                }),
                            });
                        }
                        _ => panic!("unsupported operation: {:?}", op),
                    }

                    // tmp_b is no longer needed, free it
                    self.free_tmp(tmp_b_idx);
                }
                EvaluationInstruction::CallFunction(func, args) => {
                    let arg_tmps =
                        intermediate_tmps.split_off(intermediate_tmps.len() - args.len());

                    for i in 0..args.len() {
                        let arg_tmp = self.get_tmp(arg_tmps[i]);
                        self.emit_action(Action::SetVariableToVariable {
                            first: ScoreboardEntry::new(func.with_separator('.'), args[i].clone()),
                            second: arg_tmp,
                        });
                    }

                    self.emit_action(Action::CallFunction {
                        target: func.clone(),
                    });

                    for tmp in arg_tmps {
                        self.free_tmp(tmp);
                    }

                    let ret_tmp = self.reserve_available_tmp();
                    self.emit_action(Action::SetVariableToVariable {
                        first: self.get_tmp(ret_tmp),
                        second: ScoreboardEntry::new(func.with_separator('.'), "RET".to_string()),
                    });
                    intermediate_tmps.push(ret_tmp);
                }
            }
        }

        let target_tmp = *intermediate_tmps.first().unwrap();

        if intermediate_tmps.len() == 2 {
            // the last intermediate tmp is the result of the full operation.
            // we have to move it to the first tmp, which is where the result is expected to be
            let result_tmp = *intermediate_tmps.last().unwrap();

            // optimization: except sometimes we don't need to move if the target tmp is the same as the result tmp
            if result_tmp != target_tmp {
                self.emit_action(Action::SetVariableToVariable {
                    first: self.get_tmp(target_tmp),
                    second: self.get_tmp(result_tmp),
                });
            }
        }

        for tmp in intermediate_tmps {
            self.free_tmp(tmp);
        }

        self.instructions.clear();
        target_tmp
    }

    fn emit_action(&mut self, action: Action) {
        self.actions.push(action);
    }

    fn reserve_available_tmp(&mut self) -> i32 {
        match self.available_tmps.pop() {
            Some(num) => num,
            None => {
                self.max_tmps += 1;
                self.max_tmps
            }
        }
    }

    fn free_tmp(&mut self, num: i32) {
        self.available_tmps.push(num);
    }

    fn get_tmp(&self, num: i32) -> ScoreboardEntry {
        ScoreboardEntry::new(self.scoreboard.clone(), format!("TMP{}", num))
    }

    fn local_variable(&self, str: &str) -> ScoreboardEntry {
        ScoreboardEntry::new(self.scoreboard.clone(), str.to_string())
    }
}
