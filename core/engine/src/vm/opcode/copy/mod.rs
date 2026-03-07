use super::RegisterOperand;
use crate::{
    Context, JsResult,
    vm::opcode::{OperandHandle, Operation},
};

/// `CopyDataProperties` implements the Opcode Operation for `Opcode::CopyDataProperties`
///
/// Operation:
///  - Copy all properties of one object to another object.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CopyDataProperties;

impl CopyDataProperties {
    #[inline(always)]
    pub(super) fn operation(
        (object, source, excluded_keys_handle): (
            RegisterOperand,
            RegisterOperand,
            OperandHandle<RegisterOperand>,
        ),
        context: &mut Context,
    ) -> JsResult<()> {
        let keys = context
            .vm
            .frame()
            .code_block()
            .bytecode
            .operand_arena
            .register_operands(excluded_keys_handle)
            .to_vec(); // TODO: to_vec necessary due to to_string needing owned values. we dont actually have to on most branches of to_property_key
        let object = context.vm.get_register(object.into()).clone();
        let source = context.vm.get_register(source.into()).clone();
        let mut excluded_keys = Vec::with_capacity(keys.len());
        for key in keys {
            let key = context.vm.get_register(key.into()).clone();
            excluded_keys.push(
                key.to_property_key(context)
                    .expect("key must be property key"),
            );
        }
        let object = object.as_object().expect("not an object");
        object.copy_data_properties(&source, excluded_keys, context)?;
        Ok(())
    }
}

impl Operation for CopyDataProperties {
    const NAME: &'static str = "CopyDataProperties";
    const INSTRUCTION: &'static str = "INST - CopyDataProperties";
    const COST: u8 = 6;
}
