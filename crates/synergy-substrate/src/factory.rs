//! The plugin factory: the first thing a host talks to. One factory per
//! `cdylib`, one single-component class per factory.

use std::ffi::c_void;
use std::marker::PhantomData;

use vst3::Steinberg::Vst::SDKVersionString;
use vst3::Steinberg::*;
use vst3::{uid, Class, ComWrapper};

use crate::component::PluginComponent;
use crate::strings::{copy_to_char16, copy_to_char8};
use crate::{SynergyPlugin, VENDOR, VENDOR_EMAIL, VENDOR_URL};

const AUDIO_MODULE_CATEGORY: &str = "Audio Module Class";

pub struct PluginFactory<P: SynergyPlugin> {
    _plugin: PhantomData<P>,
}

impl<P: SynergyPlugin> Class for PluginFactory<P> {
    type Interfaces = (IPluginFactory3,);
}

impl<P: SynergyPlugin> PluginFactory<P> {
    fn class_identifier() -> TUID {
        let [first_word, second_word, third_word, fourth_word] = P::description().class_identifier;
        uid(first_word, second_word, third_word, fourth_word)
    }

    fn sdk_version() -> String {
        // SDKVersionString is a NUL-terminated C string constant from the bindings.
        unsafe { std::ffi::CStr::from_ptr(SDKVersionString) }.to_string_lossy().into_owned()
    }
}

/// Creates the factory `GetPluginFactory` hands to the host (ownership passes to the host).
pub fn create_factory<P: SynergyPlugin>() -> *mut IPluginFactory {
    ComWrapper::new(PluginFactory::<P> { _plugin: PhantomData })
        .to_com_ptr::<IPluginFactory>()
        .map(|factory| factory.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

impl<P: SynergyPlugin> IPluginFactoryTrait for PluginFactory<P> {
    unsafe fn getFactoryInfo(&self, info: *mut PFactoryInfo) -> tresult {
        let Some(info) = info.as_mut() else {
            return kInvalidArgument;
        };
        copy_to_char8(VENDOR, &mut info.vendor);
        copy_to_char8(VENDOR_URL, &mut info.url);
        copy_to_char8(VENDOR_EMAIL, &mut info.email);
        info.flags = PFactoryInfo_::FactoryFlags_::kUnicode as int32;
        kResultOk
    }

    unsafe fn countClasses(&self) -> int32 {
        1
    }

    unsafe fn getClassInfo(&self, index: int32, info: *mut PClassInfo) -> tresult {
        let (0, Some(info)) = (index, info.as_mut()) else {
            return kInvalidArgument;
        };
        info.cid = Self::class_identifier();
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_to_char8(AUDIO_MODULE_CATEGORY, &mut info.category);
        copy_to_char8(P::description().name, &mut info.name);
        kResultOk
    }

    unsafe fn createInstance(&self, class_identifier: FIDString, interface_identifier: FIDString, instance: *mut *mut c_void) -> tresult {
        if class_identifier.is_null() || interface_identifier.is_null() || instance.is_null() {
            return kInvalidArgument;
        }
        *instance = std::ptr::null_mut();
        // SAFETY: VST3 passes 16-byte class/interface ids as FIDString.
        if *(class_identifier as *const TUID) != Self::class_identifier() {
            return kNoInterface;
        }
        let Some(component) = ComWrapper::new(PluginComponent::<P>::new()).to_com_ptr::<FUnknown>() else {
            return kNoInterface;
        };
        let component_pointer = component.as_ptr();
        // queryInterface adds the reference the host owns; `component` drops ours afterwards.
        ((*(*component_pointer).vtbl).queryInterface)(component_pointer, interface_identifier as *const TUID, instance)
    }
}

impl<P: SynergyPlugin> IPluginFactory2Trait for PluginFactory<P> {
    unsafe fn getClassInfo2(&self, index: int32, info: *mut PClassInfo2) -> tresult {
        let (0, Some(info)) = (index, info.as_mut()) else {
            return kInvalidArgument;
        };
        let description = P::description();
        info.cid = Self::class_identifier();
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_to_char8(AUDIO_MODULE_CATEGORY, &mut info.category);
        copy_to_char8(description.name, &mut info.name);
        // Processor and controller are one object, so the class is not distributable.
        info.classFlags = 0;
        copy_to_char8(description.sub_categories, &mut info.subCategories);
        copy_to_char8(VENDOR, &mut info.vendor);
        copy_to_char8(description.version, &mut info.version);
        copy_to_char8(&Self::sdk_version(), &mut info.sdkVersion);
        kResultOk
    }
}

impl<P: SynergyPlugin> IPluginFactory3Trait for PluginFactory<P> {
    unsafe fn getClassInfoUnicode(&self, index: int32, info: *mut PClassInfoW) -> tresult {
        let (0, Some(info)) = (index, info.as_mut()) else {
            return kInvalidArgument;
        };
        let description = P::description();
        info.cid = Self::class_identifier();
        info.cardinality = PClassInfo_::ClassCardinality_::kManyInstances as int32;
        copy_to_char8(AUDIO_MODULE_CATEGORY, &mut info.category);
        copy_to_char16(description.name, &mut info.name);
        info.classFlags = 0;
        copy_to_char8(description.sub_categories, &mut info.subCategories);
        copy_to_char16(VENDOR, &mut info.vendor);
        copy_to_char16(description.version, &mut info.version);
        copy_to_char16(&Self::sdk_version(), &mut info.sdkVersion);
        kResultOk
    }

    unsafe fn setHostContext(&self, _context: *mut FUnknown) -> tresult {
        kResultOk
    }
}
