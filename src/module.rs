//! A `Module` represets a single code compilation unit.

use llvm_sys::analysis::{LLVMVerifyModule, LLVMVerifierFailureAction};
use llvm_sys::bit_reader::{LLVMParseBitcode, LLVMParseBitcodeInContext};
use llvm_sys::bit_writer::{LLVMWriteBitcodeToFile, LLVMWriteBitcodeToMemoryBuffer};
use llvm_sys::core::{LLVMAddFunction, LLVMAddGlobal, LLVMDumpModule, LLVMGetNamedFunction, LLVMGetTypeByName, LLVMSetDataLayout, LLVMSetTarget, LLVMCloneModule, LLVMDisposeModule, LLVMGetTarget, LLVMModuleCreateWithName, LLVMGetModuleContext, LLVMGetFirstFunction, LLVMGetLastFunction, LLVMSetLinkage, LLVMAddGlobalInAddressSpace, LLVMPrintModuleToString, LLVMGetNamedMetadataNumOperands, LLVMAddNamedMetadataOperand, LLVMGetNamedMetadataOperands, LLVMGetFirstGlobal, LLVMGetLastGlobal, LLVMGetNamedGlobal, LLVMPrintModuleToFile, LLVMSetModuleInlineAsm};
use llvm_sys::execution_engine::{LLVMCreateInterpreterForModule, LLVMCreateJITCompilerForModule, LLVMCreateExecutionEngineForModule};
use llvm_sys::prelude::{LLVMValueRef, LLVMModuleRef};
use llvm_sys::LLVMLinkage;

use std::cell::{Cell, RefCell, Ref};
use std::ffi::{CString, CStr};
use std::fs::File;
use std::mem::{forget, uninitialized, zeroed};
use std::path::Path;
use std::rc::Rc;
use std::slice::from_raw_parts;

use {AddressSpace, OptimizationLevel};
use context::{Context, ContextRef};
use data_layout::DataLayout;
use execution_engine::ExecutionEngine;
use memory_buffer::MemoryBuffer;
use support::LLVMString;
use targets::Target;
use types::{AsTypeRef, BasicType, FunctionType, BasicTypeEnum};
use values::{AsValueRef, FunctionValue, GlobalValue, MetadataValue};

/// This enum defines how to link a global variable or function in a module. The variant documenation is
/// mostly taken straight from LLVM's own documentation except for some minor clarification.
///
/// It is illegal for a function declaration to have any linkage type other than external or extern_weak.
///
/// All Global Variables, Functions and Aliases can have one of the following DLL storage class: `DLLImport`
/// & `DLLExport`.
// REVIEW: Maybe this should go into it's own module?
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Linkage {
    /// `Appending` linkage may only be applied to global variables of pointer to array type. When two global
    /// variables with appending linkage are linked together, the two global arrays are appended together.
    /// This is the LLVM, typesafe, equivalent of having the system linker append together "sections" with
    /// identical names when .o files are linked. Unfortunately this doesn't correspond to any feature in .o
    /// files, so it can only be used for variables like llvm.global_ctors which llvm interprets specially.
    Appending,
    /// Globals with `AvailableExternally` linkage are never emitted into the object file corresponding to
    /// the LLVM module. From the linker's perspective, an `AvailableExternally` global is equivalent to an
    /// external declaration. They exist to allow inlining and other optimizations to take place given
    /// knowledge of the definition of the global, which is known to be somewhere outside the module. Globals
    /// with `AvailableExternally` linkage are allowed to be discarded at will, and allow inlining and other
    /// optimizations. This linkage type is only allowed on definitions, not declarations.
    AvailableExternally,
    /// `Common` linkage is most similar to "weak" linkage, but they are used for tentative definitions
    /// in C, such as "int X;" at global scope. Symbols with Common linkage are merged in the same way as
    /// weak symbols, and they may not be deleted if unreferenced. `Common` symbols may not have an explicit
    /// section, must have a zero initializer, and may not be marked 'constant'. Functions and aliases may
    /// not have `Common` linkage.
    Common,
    /// `DLLExport` causes the compiler to provide a global pointer to a pointer in a DLL, so that it can be
    /// referenced with the dllimport attribute. On Microsoft Windows targets, the pointer name is formed by
    /// combining __imp_ and the function or variable name. Since this storage class exists for defining a dll
    /// interface, the compiler, assembler and linker know it is externally referenced and must refrain from
    /// deleting the symbol.
    DLLExport,
    /// `DLLImport` causes the compiler to reference a function or variable via a global pointer to a pointer
    /// that is set up by the DLL exporting the symbol. On Microsoft Windows targets, the pointer name is
    /// formed by combining __imp_ and the function or variable name.
    DLLImport,
    /// If none of the other identifiers are used, the global is externally visible, meaning that it
    /// participates in linkage and can be used to resolve external symbol references.
    External,
    /// The semantics of this linkage follow the ELF object file model: the symbol is weak until linked,
    /// if not linked, the symbol becomes null instead of being an undefined reference.
    ExternalWeak,
    /// FIXME: Unknown linkage type
    Ghost,
    /// Similar to private, but the value shows as a local symbol (STB_LOCAL in the case of ELF) in the object
    /// file. This corresponds to the notion of the 'static' keyword in C.
    Internal,
    /// FIXME: Unknown linkage type
    LinkerPrivate,
    /// FIXME: Unknown linkage type
    LinkerPrivateWeak,
    /// Globals with `LinkOnceAny` linkage are merged with other globals of the same name when linkage occurs.
    /// This can be used to implement some forms of inline functions, templates, or other code which must be
    /// generated in each translation unit that uses it, but where the body may be overridden with a more
    /// definitive definition later. Unreferenced `LinkOnceAny` globals are allowed to be discarded. Note that
    /// `LinkOnceAny` linkage does not actually allow the optimizer to inline the body of this function into
    /// callers because it doesn’t know if this definition of the function is the definitive definition within
    /// the program or whether it will be overridden by a stronger definition. To enable inlining and other
    /// optimizations, use `LinkOnceODR` linkage.
    LinkOnceAny,
    /// FIXME: Unknown linkage type
    LinkOnceODRAutoHide,
    /// Some languages allow differing globals to be merged, such as two functions with different semantics.
    /// Other languages, such as C++, ensure that only equivalent globals are ever merged (the "one definition
    /// rule" — "ODR"). Such languages can use the `LinkOnceODR` and `WeakODR` linkage types to indicate that
    /// the global will only be merged with equivalent globals. These linkage types are otherwise the same
    /// as their non-odr versions.
    LinkOnceODR,
    /// Global values with `Private` linkage are only directly accessible by objects in the current module.
    /// In particular, linking code into a module with a private global value may cause the private to be
    /// renamed as necessary to avoid collisions. Because the symbol is private to the module, all references
    /// can be updated. This doesn’t show up in any symbol table in the object file.
    Private,
    /// `WeakAny` linkage has the same merging semantics as linkonce linkage, except that unreferenced globals
    /// with weak linkage may not be discarded. This is used for globals that are declared WeakAny in C source code.
    WeakAny,
    /// Some languages allow differing globals to be merged, such as two functions with different semantics.
    /// Other languages, such as C++, ensure that only equivalent globals are ever merged (the "one definition
    /// rule" — "ODR"). Such languages can use the `LinkOnceODR` and `WeakODR` linkage types to indicate that
    /// the global will only be merged with equivalent globals. These linkage types are otherwise the same
    /// as their non-odr versions.
    WeakODR,
}

impl Linkage {
    pub(crate) fn new(linkage: LLVMLinkage) -> Self {
        match linkage {
            LLVMLinkage::LLVMAppendingLinkage => Linkage::Appending,
            LLVMLinkage::LLVMAvailableExternallyLinkage => Linkage::AvailableExternally,
            LLVMLinkage::LLVMCommonLinkage => Linkage::Common,
            LLVMLinkage::LLVMDLLExportLinkage => Linkage::DLLExport,
            LLVMLinkage::LLVMDLLImportLinkage => Linkage::DLLImport,
            LLVMLinkage::LLVMExternalLinkage => Linkage::External,
            LLVMLinkage::LLVMExternalWeakLinkage => Linkage::ExternalWeak,
            LLVMLinkage::LLVMGhostLinkage => Linkage::Ghost,
            LLVMLinkage::LLVMInternalLinkage => Linkage::Internal,
            LLVMLinkage::LLVMLinkerPrivateLinkage => Linkage::LinkerPrivate,
            LLVMLinkage::LLVMLinkerPrivateWeakLinkage => Linkage::LinkerPrivateWeak,
            LLVMLinkage::LLVMLinkOnceAnyLinkage => Linkage::LinkOnceAny,
            LLVMLinkage::LLVMLinkOnceODRAutoHideLinkage => Linkage::LinkOnceODRAutoHide,
            LLVMLinkage::LLVMLinkOnceODRLinkage => Linkage::LinkOnceODR,
            LLVMLinkage::LLVMPrivateLinkage => Linkage::Private,
            LLVMLinkage::LLVMWeakAnyLinkage => Linkage::WeakAny,
            LLVMLinkage::LLVMWeakODRLinkage => Linkage::WeakODR,
        }
    }

    pub(crate) fn as_llvm_linkage(&self) -> LLVMLinkage {
        match *self {
            Linkage::Appending => LLVMLinkage::LLVMAppendingLinkage,
            Linkage::AvailableExternally => LLVMLinkage::LLVMAvailableExternallyLinkage,
            Linkage::Common => LLVMLinkage::LLVMCommonLinkage,
            Linkage::DLLExport => LLVMLinkage::LLVMDLLExportLinkage,
            Linkage::DLLImport => LLVMLinkage::LLVMDLLImportLinkage,
            Linkage::External => LLVMLinkage::LLVMExternalLinkage,
            Linkage::ExternalWeak => LLVMLinkage::LLVMExternalWeakLinkage,
            Linkage::Ghost => LLVMLinkage::LLVMGhostLinkage,
            Linkage::Internal => LLVMLinkage::LLVMInternalLinkage,
            Linkage::LinkerPrivate => LLVMLinkage::LLVMLinkerPrivateLinkage,
            Linkage::LinkerPrivateWeak => LLVMLinkage::LLVMLinkerPrivateWeakLinkage,
            Linkage::LinkOnceAny => LLVMLinkage::LLVMLinkOnceAnyLinkage,
            Linkage::LinkOnceODRAutoHide => LLVMLinkage::LLVMLinkOnceODRAutoHideLinkage,
            Linkage::LinkOnceODR => LLVMLinkage::LLVMLinkOnceODRLinkage,
            Linkage::Private => LLVMLinkage::LLVMPrivateLinkage,
            Linkage::WeakAny => LLVMLinkage::LLVMWeakAnyLinkage,
            Linkage::WeakODR => LLVMLinkage::LLVMWeakODRLinkage,
        }
    }
}

/// Represents a reference to an LLVM `Module`.
/// The underlying module will be disposed when dropping this object.
#[derive(Debug, PartialEq, Eq)]
pub struct Module {
    pub(crate) non_global_context: Option<Context>, // REVIEW: Could we just set context to the global context?
    data_layout: RefCell<Option<DataLayout>>,
    pub(crate) module: Cell<LLVMModuleRef>,
    pub(crate) owned_by_ee: RefCell<Option<ExecutionEngine>>,
}

impl Module {
    pub(crate) fn new(module: LLVMModuleRef, context: Option<&Context>) -> Self {
        debug_assert!(!module.is_null());

        Module {
            module: Cell::new(module),
            non_global_context: context.map(|ctx| Context::new(ctx.context.clone())),
            owned_by_ee: RefCell::new(None),
            data_layout: RefCell::new(Some(Module::get_borrowed_data_layout(module))),
        }
    }

    /// Creates a named `Module`. Will be automatically assigned the global context.
    ///
    /// To use your own `Context`, see [inkwell::context::create_module()](../context/struct.Context.html#method.create_module)
    ///
    /// # Example
    /// ```
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    ///
    /// let context = Context::get_global();
    /// let module = Module::create("my_module");
    ///
    /// assert_eq!(module.get_context(), context);
    /// ```
    pub fn create(name: &str) -> Self {
        let c_string = CString::new(name).expect("Conversion to CString failed unexpectedly");

        let module = unsafe {
            LLVMModuleCreateWithName(c_string.as_ptr())
        };

        Module::new(module, None)
    }

    /// Creates a function given its `name` and `ty`, adds it to the `Module`
    /// and returns it.
    ///
    /// An optional `linkage` can be specified, without which the default value
    /// `Linkage::ExternalLinkage` will be used.
    ///
    /// # Example
    /// ```
    /// use inkwell::context::Context;
    /// use inkwell::module::{Module, Linkage};
    ///
    /// let context = Context::get_global();
    /// let module = Module::create("my_module");
    ///
    /// let fn_type = context.f32_type().fn_type(&[], false);
    /// let fn_val = module.add_function("my_function", &fn_type, None);
    ///
    /// assert_eq!(fn_val.get_name().to_str(), Ok("my_function"));
    /// assert_eq!(fn_val.get_linkage(), Linkage::External);
    /// ```
    pub fn add_function(&self, name: &str, ty: &FunctionType, linkage: Option<Linkage>) -> FunctionValue {
        let c_string = CString::new(name).expect("Conversion to CString failed unexpectedly");

        let value = unsafe {
            LLVMAddFunction(self.module.get(), c_string.as_ptr(), ty.as_type_ref())
        };

        let fn_value = FunctionValue::new(value).expect("add_function should always succeed in adding a new function");

        if let Some(linkage) = linkage {
            fn_value.set_linkage(linkage)
        }

        fn_value
    }

    /// Gets the `Context` from which this `Module` originates.
    ///
    /// # Example
    /// ```
    /// use inkwell::context::{Context, ContextRef};
    /// use inkwell::module::Module;
    ///
    /// let global_context = Context::get_global();
    /// let global_module = Module::create("my_global_module");
    ///
    /// assert_eq!(global_module.get_context(), global_context);
    ///
    /// let local_context = Context::create();
    /// let local_module = local_context.create_module("my_module");
    ///
    /// assert_eq!(*local_module.get_context(), local_context);
    /// assert_ne!(local_context, *global_context);
    /// ```
    pub fn get_context(&self) -> ContextRef {
        let context = unsafe {
            LLVMGetModuleContext(self.module.get())
        };

        // REVIEW: This probably should be somehow using the existing context Rc
        ContextRef::new(Context::new(Rc::new(context)))
    }

    /// Gets the first `FunctionValue` defined in this `Module`.
    ///
    /// # Example
    /// ```rust,no_run
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    ///
    /// let context = Context::create();
    /// let module = context.create_module("my_mod");
    ///
    /// assert!(module.get_first_function().is_none());
    ///
    /// let void_type = context.void_type();
    /// let fn_type = void_type.fn_type(&[], false);
    /// let fn_value = module.add_function("my_fn", &fn_type, None);
    ///
    /// assert_eq!(fn_value, module.get_first_function().unwrap());
    /// ```
    pub fn get_first_function(&self) -> Option<FunctionValue> {
        let function = unsafe {
            LLVMGetFirstFunction(self.module.get())
        };

        FunctionValue::new(function)
    }

    /// Gets the last `FunctionValue` defined in this `Module`.
    ///
    /// # Example
    /// ```rust,no_run
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    ///
    /// let context = Context::create();
    /// let module = context.create_module("my_mod");
    ///
    /// assert!(module.get_last_function().is_none());
    ///
    /// let void_type = context.void_type();
    /// let fn_type = void_type.fn_type(&[], false);
    /// let fn_value = module.add_function("my_fn", &fn_type, None);
    ///
    /// assert_eq!(fn_value, module.get_last_function().unwrap());
    /// ```
    pub fn get_last_function(&self) -> Option<FunctionValue> {
        let function = unsafe {
            LLVMGetLastFunction(self.module.get())
        };

        FunctionValue::new(function)
    }

    /// Gets a `FunctionValue` defined in this `Module` by its name.
    ///
    /// # Example
    /// ```rust,no_run
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    ///
    /// let context = Context::create();
    /// let module = context.create_module("my_mod");
    ///
    /// assert!(module.get_function("my_fn").is_none());
    ///
    /// let void_type = context.void_type();
    /// let fn_type = void_type.fn_type(&[], false);
    /// let fn_value = module.add_function("my_fn", &fn_type, None);
    ///
    /// assert_eq!(fn_value, module.get_function("my_fn").unwrap());
    /// ```
    pub fn get_function(&self, name: &str) -> Option<FunctionValue> {
        let c_string = CString::new(name).expect("Conversion to CString failed unexpectedly");

        let value = unsafe {
            LLVMGetNamedFunction(self.module.get(), c_string.as_ptr())
        };

        FunctionValue::new(value)
    }


    /// Gets a `BasicTypeEnum` of a named type in a `Module`.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use inkwell::context::Context;
    ///
    /// let context = Context::create();
    /// let module = context.create_module("my_module");
    ///
    /// assert!(module.get_type("foo").is_none());
    ///
    /// let opaque = context.opaque_struct_type("foo");
    ///
    /// assert_eq!(module.get_type("foo").unwrap(), opaque.into());
    /// ```
    pub fn get_type(&self, name: &str) -> Option<BasicTypeEnum> {
        let c_string = CString::new(name).expect("Conversion to CString failed unexpectedly");

        let type_ = unsafe {
            LLVMGetTypeByName(self.module.get(), c_string.as_ptr())
        };

        if type_.is_null() {
            return None;
        }

        Some(BasicTypeEnum::new(type_))
    }

    /// Sets a `Target` to this `Module`.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use inkwell::context::Context;
    /// use inkwell::targets::Target;
    ///
    /// Target::initialize_x86(&Default::default());
    ///
    /// let context = Context::create();
    /// let module = context.create_module("mod");
    /// let target = Target::from_name("x86-64").unwrap();
    ///
    /// assert!(module.get_target().is_none());
    ///
    /// module.set_target(&target);
    ///
    /// assert_eq!(module.get_target().unwrap(), target);
    /// ```
    pub fn set_target(&self, target: &Target) {
        unsafe {
            LLVMSetTarget(self.module.get(), target.get_name().as_ptr())
        }
    }

    /// Gets the `Target` assigned to this `Module`, if any.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use inkwell::context::Context;
    /// use inkwell::targets::Target;
    ///
    /// Target::initialize_x86(&Default::default());
    ///
    /// let context = Context::create();
    /// let module = context.create_module("mod");
    /// let target = Target::from_name("x86-64").unwrap();
    ///
    /// assert!(module.get_target().is_none());
    ///
    /// module.set_target(&target);
    ///
    /// assert_eq!(module.get_target().unwrap(), target);
    /// ```
    pub fn get_target(&self) -> Option<Target> {
        // REVIEW: This isn't an owned LLVMString, is it? If so, need to deallocate.
        let target_str = unsafe {
            LLVMGetTarget(self.module.get())
        };

        Target::from_name_raw(target_str)
    }

    /// Creates an `ExecutionEngine` from this `Module`.
    ///
    /// # Example
    /// ```no_run
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    /// use inkwell::targets::{InitializationConfig, Target};
    ///
    /// Target::initialize_native(&InitializationConfig::default()).expect("Failed to initialize native target");
    ///
    /// let context = Context::get_global();
    /// let module = Module::create("my_module");
    /// let execution_engine = module.create_execution_engine().unwrap();
    ///
    /// assert_eq!(module.get_context(), context);
    /// ```
    // SubType: ExecutionEngine<?>
    pub fn create_execution_engine(&self) -> Result<ExecutionEngine, LLVMString> {
        let mut execution_engine = unsafe { zeroed() };
        let mut err_string = unsafe { zeroed() };
        let code = unsafe {
            LLVMCreateExecutionEngineForModule(&mut execution_engine, self.module.get(), &mut err_string) // Takes ownership of module
        };

        if code == 1 {
            return Err(LLVMString::new(err_string));
        }

        let execution_engine = ExecutionEngine::new(Rc::new(execution_engine), false);

        *self.owned_by_ee.borrow_mut() = Some(execution_engine.clone());

        Ok(execution_engine)
    }

    /// Creates an interpreter `ExecutionEngine` from this `Module`.
    ///
    /// # Example
    /// ```no_run
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    /// use inkwell::targets::{InitializationConfig, Target};
    ///
    /// Target::initialize_native(&InitializationConfig::default()).expect("Failed to initialize native target");
    ///
    /// let context = Context::get_global();
    /// let module = Module::create("my_module");
    /// let execution_engine = module.create_interpreter_execution_engine().unwrap();
    ///
    /// assert_eq!(module.get_context(), context);
    /// ```
    // SubType: ExecutionEngine<Interpreter>
    pub fn create_interpreter_execution_engine(&self) -> Result<ExecutionEngine, LLVMString> {
        let mut execution_engine = unsafe { uninitialized() };
        let mut err_string = unsafe { zeroed() };

        let code = unsafe {
            LLVMCreateInterpreterForModule(&mut execution_engine, self.module.get(), &mut err_string) // Takes ownership of module
        };

        if code == 1 {
            return Err(LLVMString::new(err_string));
        }

        let execution_engine = ExecutionEngine::new(Rc::new(execution_engine), false);

        *self.owned_by_ee.borrow_mut() = Some(execution_engine.clone());

        Ok(execution_engine)
    }

    /// Creates a JIT `ExecutionEngine` from this `Module`.
    ///
    /// # Example
    /// ```no_run
    /// use inkwell::OptimizationLevel;
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    /// use inkwell::targets::{InitializationConfig, Target};
    ///
    /// Target::initialize_native(&InitializationConfig::default()).expect("Failed to initialize native target");
    ///
    /// let context = Context::get_global();
    /// let module = Module::create("my_module");
    /// let execution_engine = module.create_jit_execution_engine(OptimizationLevel::None).unwrap();
    ///
    /// assert_eq!(module.get_context(), context);
    /// ```
    // SubType: ExecutionEngine<Jit>
    pub fn create_jit_execution_engine(&self, opt_level: OptimizationLevel) -> Result<ExecutionEngine, LLVMString> {
        let mut execution_engine = unsafe { uninitialized() };
        let mut err_string = unsafe { zeroed() };

        let code = unsafe {
            LLVMCreateJITCompilerForModule(&mut execution_engine, self.module.get(), opt_level as u32, &mut err_string) // Takes ownership of module
        };

        if code == 1 {
            // The module still seems "owned" in this error case, despite failing to create an EE. This would normally
            // end in a segfault on Module drop, however we're avoiding that by cloning the module and replacing the underlying pointer
            // REVIEW: Ensure this doesn't lead to unexpected behavior... If it does, the alternate strategy would be to change the fn
            // signature to take ownership of self and return it with good EE: (self, opt_level) -> Result<(Module, EE), LLVMString>
            let module = self.clone();

            self.module.set(module.module.get());

            forget(module);

            // REVIEW: Module still seems "owned" in the error case and may segfault on module drop. :/
            // Need to figure out if there's a way to prevent this.
            return Err(LLVMString::new(err_string));
        }

        let execution_engine = ExecutionEngine::new(Rc::new(execution_engine), true);

        *self.owned_by_ee.borrow_mut() = Some(execution_engine.clone());

        Ok(execution_engine)
    }

    /// Creates a `GlobalValue` based on a type in an address space.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use inkwell::AddressSpace;
    /// use inkwell::context::Context;
    ///
    /// let context = Context::create();
    /// let module = context.create_module("mod");
    /// let i8_type = context.i8_type();
    /// let global = module.add_global(i8_type, Some(AddressSpace::Const), "my_global");
    ///
    /// assert_eq!(module.get_first_global().unwrap(), global);
    /// assert_eq!(module.get_last_global().unwrap(), global);
    /// ```
    pub fn add_global<T: BasicType>(&self, type_: T, address_space: Option<AddressSpace>, name: &str) -> GlobalValue {
        let c_string = CString::new(name).expect("Conversion to CString failed unexpectedly");

        let value = unsafe {
            match address_space {
                Some(address_space) => LLVMAddGlobalInAddressSpace(self.module.get(), type_.as_type_ref(), c_string.as_ptr(), address_space as u32),
                None => LLVMAddGlobal(self.module.get(), type_.as_type_ref(), c_string.as_ptr()),
            }
        };

        GlobalValue::new(value)
    }

    pub fn write_bitcode_to_path(&self, path: &Path) -> bool {
        let path_str = path.to_str().expect("Did not find a valid Unicode path string");
        let c_string = CString::new(path_str).expect("Conversion to CString failed unexpectedly");

        unsafe {
            LLVMWriteBitcodeToFile(self.module.get(), c_string.as_ptr()) == 0
        }
    }

    // See GH issue #6
    #[cfg(unix)]
    pub fn write_bitcode_to_file(&self, file: &File, should_close: bool, unbuffered: bool) -> bool {
        use std::os::unix::io::AsRawFd;
        use llvm_sys::bit_writer::LLVMWriteBitcodeToFD;

        // REVIEW: as_raw_fd docs suggest it only works in *nix
        // Also, should_close should maybe be hardcoded to true?
        unsafe {
            LLVMWriteBitcodeToFD(self.module.get(), file.as_raw_fd(), should_close as i32, unbuffered as i32) == 0
        }
    }

    #[cfg(windows)]
    #[allow(unused_variables)]
    pub fn write_bitcode_to_file(&self, file: &File, should_close: bool, unbuffered: bool) -> bool {
        false
    }

    pub fn write_bitcode_to_memory(&self) -> MemoryBuffer {
        let memory_buffer = unsafe {
            LLVMWriteBitcodeToMemoryBuffer(self.module.get())
        };

        MemoryBuffer::new(memory_buffer)
    }

    /// Ensures that the current `Module` is valid, and returns a `Result`
    /// that describes whether or not it is, returning a LLVM allocated string on error.
    ///
    /// # Remarks
    /// See also: http://llvm.org/doxygen/Analysis_2Analysis_8cpp_source.html
    pub fn verify(&self) -> Result<(), LLVMString> {
        let mut err_str = unsafe { zeroed() };

        let action = LLVMVerifierFailureAction::LLVMReturnStatusAction;

        let code = unsafe {
            LLVMVerifyModule(self.module.get(), action, &mut err_str)
        };

        if code == 1 && !err_str.is_null() {
            return Err(LLVMString::new(err_str));
        }

        Ok(())
    }

    fn get_borrowed_data_layout(module: LLVMModuleRef) -> DataLayout {
        #[cfg(any(feature = "llvm3-6", feature = "llvm3-7", feature = "llvm3-8"))]
        let data_layout = unsafe {
            use llvm_sys::core::LLVMGetDataLayout;

            LLVMGetDataLayout(module)
        };
        #[cfg(not(any(feature = "llvm3-6", feature = "llvm3-7", feature = "llvm3-8")))]
        let data_layout = unsafe {
            use llvm_sys::core::LLVMGetDataLayoutStr;

            LLVMGetDataLayoutStr(module)
        };

        DataLayout::new_borrowed(data_layout)
    }

    pub fn get_data_layout(&self) -> Ref<DataLayout> {
        Ref::map(self.data_layout.borrow(), |l| l.as_ref().expect("DataLayout should always exist until Drop"))
    }

    // REVIEW: Ensure the replaced string ptr still gets cleaned up by the module (I think it does)
    // valgrind might come in handy once non jemalloc allocators stabilize
    pub fn set_data_layout(&self, data_layout: &DataLayout) {
        unsafe {
            LLVMSetDataLayout(self.module.get(), data_layout.as_ptr());
        }

        *self.data_layout.borrow_mut() = Some(Module::get_borrowed_data_layout(self.module.get()));
    }

    /// Prints the content of the `Module` to stderr.
    pub fn print_to_stderr(&self) {
        unsafe {
            LLVMDumpModule(self.module.get());
        }
    }

    /// Prints the content of the `Module` to a string.
    pub fn print_to_string(&self) -> LLVMString {
        let module_string = unsafe {
            LLVMPrintModuleToString(self.module.get())
        };

        LLVMString::new(module_string)
    }

    /// Prints the content of the `Module` to a file.
    pub fn print_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), LLVMString> {
        let path = path.as_ref().to_str().expect("Did not find a valid Unicode path string");
        let mut err_string = unsafe { zeroed() };
        let return_code = unsafe {
            LLVMPrintModuleToFile(self.module.get(), path.as_ptr() as *const i8, &mut err_string)
        };

        // TODO: Verify 1 is error code (LLVM can be inconsistent)
        if return_code == 1 {
            return Err(LLVMString::new(err_string));
        }

        Ok(())
    }

    pub fn set_inline_assembly(&self, asm: &str) {
        let c_string = CString::new(asm).expect("Conversion to CString failed unexpectedly");

        unsafe {
            LLVMSetModuleInlineAsm(self.module.get(), c_string.as_ptr())
        }
    }

    // REVIEW: Should module take ownership of metadata?
    // REVIEW: Should we return a MetadataValue for the global since it's its own value?
    // it would be the last item in get_global_metadata I believe
    // TODOC: Appends your metadata to a global MetadataValue<Node> indexed by key
    pub fn add_global_metadata(&self, key: &str, metadata: &MetadataValue) {
        let c_string = CString::new(key).expect("Conversion to CString failed unexpectedly");

        unsafe {
            LLVMAddNamedMetadataOperand(self.module.get(), c_string.as_ptr(), metadata.as_value_ref())
        }
    }
    // REVIEW: Better name?
    // TODOC: Gets the size of the metadata node indexed by key
    pub fn get_global_metadata_size(&self, key: &str) -> u32 {
        let c_string = CString::new(key).expect("Conversion to CString failed unexpectedly");

        unsafe {
            LLVMGetNamedMetadataNumOperands(self.module.get(), c_string.as_ptr())
        }
    }

    // TODOC: Always returns a metadata node indexed by key, which may contain 1 string or multiple values as its get_node_values()
    // SubTypes: -> Vec<MetadataValue<Node>>
    pub fn get_global_metadata(&self, key: &str) -> Vec<MetadataValue> {
        let c_string = CString::new(key).expect("Conversion to CString failed unexpectedly");
        let count = self.get_global_metadata_size(key);

        let mut raw_vec: Vec<LLVMValueRef> = Vec::with_capacity(count as usize);
        let ptr = raw_vec.as_mut_ptr();

        forget(raw_vec);

        let slice = unsafe {
            LLVMGetNamedMetadataOperands(self.module.get(), c_string.as_ptr(), ptr);

            from_raw_parts(ptr, count as usize)
        };

        slice.iter().map(|val| MetadataValue::new(*val)).collect()
    }

    /// Gets the first `GlobalValue` in a module.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use inkwell::AddressSpace;
    /// use inkwell::context::Context;
    ///
    /// let context = Context::create();
    /// let i8_type = context.i8_type();
    /// let module = context.create_module("mod");
    ///
    /// assert!(module.get_first_global().is_none());
    ///
    /// let global = module.add_global(i8_type, Some(AddressSpace::Const), "my_global");
    ///
    /// assert_eq!(module.get_first_global().unwrap(), global);
    /// ```
    pub fn get_first_global(&self) -> Option<GlobalValue> {
        let value = unsafe {
            LLVMGetFirstGlobal(self.module.get())
        };

        if value.is_null() {
            return None;
        }

        Some(GlobalValue::new(value))
    }

    /// Gets the last `GlobalValue` in a module.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use inkwell::AddressSpace;
    /// use inkwell::context::Context;
    ///
    /// let context = Context::create();
    /// let module = context.create_module("mod");
    /// let i8_type = context.i8_type();
    ///
    /// assert!(module.get_last_global().is_none());
    ///
    /// let global = module.add_global(i8_type, Some(AddressSpace::Const), "my_global");
    ///
    /// assert_eq!(module.get_last_global().unwrap(), global);
    /// ```
    pub fn get_last_global(&self) -> Option<GlobalValue> {
        let value = unsafe {
            LLVMGetLastGlobal(self.module.get())
        };

        if value.is_null() {
            return None;
        }

        Some(GlobalValue::new(value))
    }

    /// Gets a named `GlobalValue` in a module.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use inkwell::AddressSpace;
    /// use inkwell::context::Context;
    ///
    /// let context = Context::create();
    /// let module = context.create_module("mod");
    /// let i8_type = context.i8_type();
    ///
    /// assert!(module.get_global("my_global").is_none());
    ///
    /// let global = module.add_global(i8_type, Some(AddressSpace::Const), "my_global");
    ///
    /// assert_eq!(module.get_global("my_global").unwrap(), global);
    /// ```
    pub fn get_global(&self, name: &str) -> Option<GlobalValue> {
        let c_string = CString::new(name).expect("Conversion to CString failed unexpectedly");
        let value = unsafe {
            LLVMGetNamedGlobal(self.module.get(), c_string.as_ptr())
        };

        if value.is_null() {
            return None;
        }

        Some(GlobalValue::new(value))
    }

    /// Creates a new `Module` from a `MemoryBuffer`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    /// use inkwell::memory_buffer::MemoryBuffer;
    /// use std::path::Path;
    ///
    /// let path = Path::new("foo/bar.bc");
    /// let buffer = MemoryBuffer::create_from_file(&path).unwrap();
    /// let module = Module::parse_bitcode_from_buffer(&buffer);
    ///
    /// assert_eq!(module.unwrap().get_context(), Context::get_global());
    ///
    /// ```
    pub fn parse_bitcode_from_buffer(buffer: &MemoryBuffer) -> Result<Self, LLVMString> {
        let mut module = unsafe { zeroed() };
        let mut err_string = unsafe { zeroed() };

        // LLVM has a newer version of this function w/o the error result since 3.8 but this deprecated function
        // hasen't yet been removed even in the unreleased LLVM 7. Seems fine to use instead of switching to their
        // error diagnostics handler
        let success = unsafe {
            LLVMParseBitcode(buffer.memory_buffer, &mut module, &mut err_string)
        };

        if success != 0 {
            return Err(LLVMString::new(err_string));
        }

        Ok(Module::new(module, None))
    }

    /// Creates a new `Module` from a `MemoryBuffer`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    /// use inkwell::memory_buffer::MemoryBuffer;
    /// use std::path::Path;
    ///
    /// let path = Path::new("foo/bar.bc");
    /// let context = Context::create();
    /// let buffer = MemoryBuffer::create_from_file(&path).unwrap();
    /// let module = Module::parse_bitcode_from_buffer_in_context(&buffer, &context);
    ///
    /// assert_eq!(module.unwrap().get_context(), Context::get_global());
    ///
    /// ```
    pub fn parse_bitcode_from_buffer_in_context(buffer: &MemoryBuffer, context: &Context) -> Result<Self, LLVMString> {
        let mut module = unsafe { zeroed() };
        let mut err_string = unsafe { zeroed() };

        // LLVM has a newer version of this function w/o the error result since 3.8 but this deprecated function
        // hasen't yet been removed even in the unreleased LLVM 7. Seems fine to use instead of switching to their
        // error diagnostics handler
        let success = unsafe {
            LLVMParseBitcodeInContext(*context.context, buffer.memory_buffer, &mut module, &mut err_string)
        };

        if success != 0 {
            return Err(LLVMString::new(err_string));
        }

        Ok(Module::new(module, Some(&context)))
    }

    /// A convenience function for creating a `Module` from a file.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    /// use std::path::Path;
    ///
    /// let path = Path::new("foo/bar.bc");
    /// let module = Module::parse_bitcode_from_path(&path);
    ///
    /// assert_eq!(module.unwrap().get_context(), Context::get_global());
    ///
    /// ```
    // LLVMGetBitcodeModule was a pain to use, so I seem to be able to achieve the same effect
    // by reusing create_from_file instead. This is basically just a convenience function.
    pub fn parse_bitcode_from_path<P: AsRef<Path>>(path: P) -> Result<Self, LLVMString> {
        let buffer = MemoryBuffer::create_from_file(path.as_ref())?;

        Self::parse_bitcode_from_buffer(&buffer)
    }

    /// A convenience function for creating a `Module` from a file for a given context.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use inkwell::context::Context;
    /// use inkwell::module::Module;
    /// use std::path::Path;
    ///
    /// let path = Path::new("foo/bar.bc");
    /// let context = Context::create();
    /// let module = Module::parse_bitcode_from_path_in_context(&path, &context);
    ///
    /// assert_eq!(*module.unwrap().get_context(), context);
    ///
    /// ```
    // LLVMGetBitcodeModuleInContext was a pain to use, so I seem to be able to achieve the same effect
    // by reusing create_from_file instead. This is basically just a convenience function.
    pub fn parse_bitcode_from_path_in_context<P: AsRef<Path>>(path: P, context: &Context) -> Result<Self, LLVMString> {
        let buffer = MemoryBuffer::create_from_file(path.as_ref())?;

        Self::parse_bitcode_from_buffer_in_context(&buffer, &context)
    }
}

impl Clone for Module {
    fn clone(&self) -> Self {
        // REVIEW: Is this just a LLVM 6 bug? We could conditionally compile this assertion for affected versions
        let verify = self.verify();

        assert!(verify.is_ok(), "Cloning a Module seems to segfault when module is not valid. We are preventing that here. Error: {}", verify.unwrap_err());

        let module = unsafe {
            LLVMCloneModule(self.module.get())
        };

        Module::new(module, self.non_global_context.as_ref())
    }
}

// Module owns the data layout string, so LLVMDisposeModule will deallocate it for us.
// which is why DataLayout must be called with `new_borrowed`
impl Drop for Module {
    fn drop(&mut self) {
        if self.owned_by_ee.borrow_mut().take().is_none() {
            unsafe {
                 LLVMDisposeModule(self.module.get());
            }
        }

        // Context & EE will drop naturally if they are unique references at this point
    }
}
