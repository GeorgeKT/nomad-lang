use std::ptr;
use std::rc::Rc;
use std::ffi::CStr;
use std::os::raw::c_char;
use llvm::core::*;
use llvm::prelude::*;
use llvm::analysis::*;
use libc;

use ast::*;
use codegen::*;
use compileerror::*;

pub unsafe fn type_name(tr: LLVMTypeRef) -> String
{
    let n = LLVMPrintTypeToString(tr);
    let name = CStr::from_ptr(n).to_str().expect("Invalid C String").to_owned();
    LLVMDisposeMessage(n);
    name
}

#[allow(unused_variables)]
fn gen_import(ctx: &mut Context, import: &Import) -> Result<(), CompileError>
{
     err(Pos::new(0, 0), ErrorType::UnexpectedEOF)
}

unsafe fn gen_variable(ctx: &mut Context, v: &Variable) -> Result<(), CompileError>
{
    if ctx.has_variable(&v.name) {
        return err(v.span.start, ErrorType::RedefinitionOfVariable(v.name.clone()));
    }

    let initial_value = try!(gen_expression(ctx, &v.init));
    let initial_value_type = LLVMTypeOf(initial_value);

    let v_typ = if v.typ == Type::Unknown {
        try!(ctx.infer_type(&v.init))
    } else {
        v.typ.clone()
    };

    if let Some(llvm_type_ref) = ctx.resolve_type(&v_typ) {
        if llvm_type_ref != initial_value_type {
            return err(v.span.start, ErrorType::TypeError(format!("Mismatched types in initialization ({} vs {})",
                type_name(llvm_type_ref), type_name(initial_value_type))));
        }
    } else {
        return err(v.span.start, ErrorType::TypeError(format!("Unknown type '{}'", v.typ)));
    }


    let var = LLVMBuildAlloca(ctx.builder, initial_value_type, cstr("var"));
    LLVMBuildStore(ctx.builder, initial_value, var);

    let mut sf = ctx.top_stack_frame();
    sf.add_variable(&v.name, var, v.is_const, v_typ);
    Ok(())
}

unsafe fn gen_function_sig(ctx: &mut Context, sig: &FunctionSignature, public: bool, span: &Span) -> Result<FunctionInstance, CompileError>
{
    let ret_type = try!(ctx
        .resolve_type(&sig.return_type)
        .ok_or(CompileError::new(span.start, ErrorType::TypeError(format!("Cannot resolve the return type of function '{}'", sig.name)))));

    let mut arg_types = Vec::new();
    for arg in &sig.args {
        let arg_type = try!(ctx
            .resolve_type(&arg.typ)
            .ok_or(CompileError::new(arg.span.start, ErrorType::TypeError(format!("Cannot resolve the type of argument '{}'", arg.name)))));
        arg_types.push(arg_type);
    }

    let function_type = LLVMFunctionType(ret_type, arg_types.as_mut_ptr(), arg_types.len() as libc::c_uint, 0);
    let function = LLVMAddFunction(ctx.module, cstr(&sig.name), function_type);

    Ok(FunctionInstance{
        name: sig.name.clone(),
        args: arg_types,
        return_type: ret_type,
        function: function,
        sig: sig.clone(),
        public: public,
    })
}

unsafe fn gen_function(ctx: &mut Context, f: &Function) -> Result<FunctionInstance, CompileError>
{
    if ctx.has_function(&f.sig.name) {
        return err(f.span.start, ErrorType::RedefinitionOfFunction(f.sig.name.clone()));
    }

    let fi = try!(gen_function_sig(ctx, &f.sig, f.public, &f.span));

    let bb = LLVMAppendBasicBlockInContext(ctx.context, fi.function, cstr("entry"));
    LLVMPositionBuilderAtEnd(ctx.builder, bb);

    ctx.push_stack_frame(fi.function, bb);

    for (i, arg) in f.sig.args.iter().enumerate() {
        let var = LLVMGetParam(fi.function, i as libc::c_uint);
        let alloc = LLVMBuildAlloca(ctx.builder, fi.args[i], cstr("argtmp"));
        LLVMBuildStore(ctx.builder, var, alloc);
        ctx.top_stack_frame().add_variable(&arg.name, alloc, arg.constant, arg.typ.clone());
    }

    for s in &f.block.statements {
        try!(gen_statement(ctx, s));
    }

    if f.sig.return_type == Type::Void {
        LLVMBuildRetVoid(ctx.builder);
    }

    ctx.pop_stack_frame();
    LLVMPositionBuilderAtEnd(ctx.builder, ctx.top_stack_frame().get_current_bb());
    Ok(fi)
}

unsafe fn gen_external_function(ctx: &mut Context, f: &ExternalFunction) -> Result<(), CompileError>
{
    let fi = try!(gen_function_sig(ctx, &f.sig, true, &f.span));
    ctx.top_stack_frame().add_function(fi);
    Ok(())
}

unsafe fn gen_block(ctx: &mut Context, b: &Block) -> Result<(), CompileError>
{
    for s in &b.statements {
        try!(gen_statement(ctx, s));
    }
    Ok(())
}

unsafe fn gen_while(ctx: &mut Context, f: &While) -> Result<(), CompileError>
{
    let func = ctx.top_stack_frame().get_current_function();
    let loop_cond_bb = LLVMAppendBasicBlockInContext(ctx.context, func, cstr("loop_cond"));
    let loop_body_bb = LLVMAppendBasicBlockInContext(ctx.context, func, cstr("loop_body"));
    let post_loop_bb = LLVMAppendBasicBlockInContext(ctx.context, func, cstr("loop_done"));

    LLVMBuildBr(ctx.builder, loop_cond_bb);
    LLVMPositionBuilderAtEnd(ctx.builder, loop_cond_bb);
    let cond = try!(gen_expression(ctx, &f.cond));
    LLVMBuildCondBr(ctx.builder, cond, loop_body_bb, post_loop_bb);
    LLVMPositionBuilderAtEnd(ctx.builder, loop_body_bb);
    ctx.top_stack_frame().set_current_bb(loop_body_bb);

    try!(gen_block(ctx, &f.block));

    LLVMBuildBr(ctx.builder, loop_cond_bb);
    LLVMPositionBuilderAtEnd(ctx.builder, post_loop_bb);
    ctx.top_stack_frame().set_current_bb(post_loop_bb);
    Ok(())
}

unsafe fn gen_if(ctx: &mut Context, f: &If) -> Result<(), CompileError>
{
    let func = ctx.top_stack_frame().get_current_function();
    let if_bb = LLVMAppendBasicBlockInContext(ctx.context, func, cstr("if_bb"));
    let after_if_bb = LLVMAppendBasicBlockInContext(ctx.context, func, cstr("after_if_bb"));
    let else_bb = LLVMAppendBasicBlockInContext(ctx.context, func, cstr("else_bb"));
    let cond = try!(gen_expression(ctx, &f.cond));

    LLVMBuildCondBr(ctx.builder, cond, if_bb, else_bb);
    LLVMPositionBuilderAtEnd(ctx.builder, if_bb);

    try!(gen_block(ctx, &f.if_block));
    LLVMBuildBr(ctx.builder, after_if_bb);

    match f.else_part {
        ElsePart::Block(ref else_block) => {
            LLVMPositionBuilderAtEnd(ctx.builder, else_bb);
            try!(gen_block(ctx, else_block));
            LLVMBuildBr(ctx.builder, after_if_bb);
        },
        ElsePart::Empty => {
            LLVMPositionBuilderAtEnd(ctx.builder, else_bb);
            LLVMBuildBr(ctx.builder, after_if_bb);
        },
        ElsePart::If(ref next_if) => {
            LLVMPositionBuilderAtEnd(ctx.builder, else_bb);
            try!(gen_if(ctx, next_if));
            LLVMBuildBr(ctx.builder, after_if_bb);
        }
    }

    LLVMPositionBuilderAtEnd(ctx.builder, after_if_bb);
    ctx.top_stack_frame().set_current_bb(after_if_bb);
    Ok(())
}

unsafe fn gen_return(ctx: &mut Context, f: &Return) -> Result<(), CompileError>
{
    let ret = try!(gen_expression(ctx, &f.expr));
    let builder = ctx.builder;
    let sf = ctx.top_stack_frame();
    let ret_type =  LLVMTypeOf(ret);
    let func_type = sf.return_type();
    if ret_type != func_type {
        err(f.span.start, ErrorType::TypeError(
            format!("Attempting to return type '{}' expecting '{}'", type_name(ret_type), type_name(func_type))))
    } else {
        LLVMBuildRet(builder, ret);
        Ok(())
    }
}

unsafe fn gen_struct(ctx: &mut Context, s: &Struct) -> Result<(), CompileError>
{
    if let Some(_) = ctx.get_complex_type(&s.name) {
        return err(s.span.start, ErrorType::RedefinitionOfStruct(s.name.clone()));
    }

    let mut members = Vec::with_capacity(s.variables.len());
    let mut element_types = Vec::with_capacity(s.variables.len());
    for v in &s.variables
    {
        let typ = if v.typ == Type::Unknown {
            try!(ctx.infer_type(&v.init))
        } else {
            v.typ.clone()
        };

         if let Some(llvm_typ) = ctx.resolve_type(&typ) {
            members.push(Rc::new(StructMemberVar{
                name: v.name.clone(),
                typ: typ,
                llvm_typ: llvm_typ,
                constant: v.is_const,
                public: v.public,
                init: v.init.clone(),
            }));
            element_types.push(llvm_typ);
         } else {
            return err(v.span.start, ErrorType::TypeError(
                format!("Unable to determine type of member '{}' of struct '{}'", v.name, s.name)));
        }
    }

    let struct_type = StructType{
        name: s.name.clone(),
        typ: LLVMStructTypeInContext(ctx.context, element_types.as_mut_ptr(), s.variables.len() as u32, 0),
        members: members,
    };

    ctx.top_stack_frame().add_complex_type(struct_type);

    for f in &s.functions {
        let func = try!(gen_function(ctx, f));
        ctx.top_stack_frame().add_function(func)
    }

    Ok(())
}

#[allow(unused_variables)]
fn gen_union(ctx: &mut Context, f: &Union) -> Result<(), CompileError>
{
     err(Pos::new(0, 0), ErrorType::UnexpectedEOF)
}

#[allow(unused_variables)]
fn gen_match(ctx: &mut Context, f: &Match) -> Result<(), CompileError>
{
     err(Pos::new(0, 0), ErrorType::UnexpectedEOF)
}

unsafe fn gen_statement(ctx: &mut Context, stmt: &Statement) -> Result<(), CompileError>
{
    match *stmt {
        Statement::Import(ref i) => gen_import(ctx, i),
        Statement::Variable(ref vars) => {
            for v in vars {
                try!(gen_variable(ctx, v))
            }
            Ok(())
        },
        Statement::Function(ref fun) => {
            let function_instance = try!(gen_function(ctx, fun));
            ctx.top_stack_frame().add_function(function_instance);
            Ok(())
        },
        Statement::ExternalFunction(ref fun) => gen_external_function(ctx, fun),
        Statement::While(ref w) => gen_while(ctx, w),
        Statement::If(ref i) => gen_if(ctx, i),
        Statement::Return(ref r) => gen_return(ctx, r),
        Statement::Struct(ref s) => gen_struct(ctx, s),
        Statement::Union(ref u) => gen_union(ctx, u),
        Statement::Match(ref m) => gen_match(ctx, m),
        Statement::Expression(ref e) => gen_expression(ctx, e).map(|_| ()),
    }
}

pub unsafe fn verify_module(ctx: &Context) -> Result<(), CompileError>
{
    let mut error_message: *mut c_char = ptr::null_mut();
    if LLVMVerifyModule(ctx.module, LLVMVerifierFailureAction::LLVMReturnStatusAction, &mut error_message) != 0 {
        let msg = CStr::from_ptr(error_message).to_str().expect("Invalid C string");
        let e = format!("Module verification error: {}", msg);
        LLVMDisposeMessage(error_message);
        err(Pos::zero(), ErrorType::CodegenError(e))
    } else {
        Ok(())
    }
}

pub unsafe fn gen_program(ctx: &mut Context, prog: &Program) -> Result<(), CompileError>
{
    let main_ret_type = LLVMInt64TypeInContext(ctx.context);
    let function_type = LLVMFunctionType(main_ret_type, ptr::null_mut(), 0, 0);
    let function = LLVMAddFunction(ctx.module, cstr("main"), function_type);
    let bb = LLVMAppendBasicBlockInContext(ctx.context, function, cstr("entry"));
    LLVMPositionBuilderAtEnd(ctx.builder, bb);

    ctx.push_stack_frame(function, bb);
    try!(gen_block(ctx, &prog.block));

    if LLVMIsATerminatorInst(LLVMGetLastInstruction(ctx.top_stack_frame().get_current_bb())) == ptr::null_mut() {
        LLVMBuildRet(ctx.builder, const_int(ctx.context, 0));
    }

    try!(verify_module(ctx));
    Ok(())
}
