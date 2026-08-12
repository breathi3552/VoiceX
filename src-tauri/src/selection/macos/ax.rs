//! Thin, self-declared bindings over the Accessibility C API.
//!
//! Only the handful of `ApplicationServices` entry points the selection reader
//! needs are declared here; AX handles never escape this module.
//!
//! Every call must happen on the selection worker thread (see
//! [`crate::selection`]) — the AX API is single-threaded.

use core_foundation::base::{CFGetTypeID, CFRelease, CFTypeID, CFTypeRef, TCFType};
use core_foundation::string::{CFString, CFStringRef};

use crate::selection::Sensitivity;

#[allow(non_camel_case_types)]
type AXError = i32;

const K_AX_ERROR_SUCCESS: AXError = 0;
const K_AX_ERROR_ATTRIBUTE_UNSUPPORTED: AXError = -25205;
const K_AX_ERROR_NO_VALUE: AXError = -25212;
const K_AX_ERROR_API_DISABLED: AXError = -25211;
const K_AX_ERROR_NOT_IMPLEMENTED: AXError = -25208;

#[repr(C)]
struct __AXUIElement {
    _private: [u8; 0],
}
type AXUIElementRef = *const __AXUIElement;

#[allow(non_snake_case)]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementGetTypeID() -> CFTypeID;
}

#[allow(non_snake_case)]
#[link(name = "Carbon", kind = "framework")]
extern "C" {
    /// True while any process has enabled secure keyboard entry (password
    /// fields, Terminal's "Secure Keyboard Entry", …). Synthetic key events are
    /// blocked in that state, and the focused content is by definition secret.
    fn IsSecureEventInputEnabled() -> bool;
}

pub fn secure_event_input_enabled() -> bool {
    unsafe { IsSecureEventInputEnabled() }
}

/// Owned `AXUIElementRef`. Releases on drop.
struct AxElement(AXUIElementRef);

impl Drop for AxElement {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0 as CFTypeRef) };
        }
    }
}

/// What an attribute read produced. `Unsupported` and `Empty` are deliberately
/// distinct: the former means "ask the next layer", the latter means "this
/// control is authoritative and nothing is selected".
pub enum AttributeRead {
    Text(String),
    Empty,
    Unsupported,
    ApiDisabled,
}

pub struct FocusedElement {
    element: AxElement,
    role: Option<String>,
    subrole: Option<String>,
}

impl FocusedElement {
    /// Read the system-wide focused UI element.
    ///
    /// `Ok(None)` means the API answered but there is no focused element (for
    /// example nothing is frontmost); `Err(true)` signals the API is disabled,
    /// i.e. the accessibility permission is missing.
    pub fn read() -> Result<Option<Self>, ApiDisabled> {
        let system_wide = unsafe { AXUIElementCreateSystemWide() };
        if system_wide.is_null() {
            return Ok(None);
        }
        let system_wide = AxElement(system_wide);

        let attribute = CFString::new("AXFocusedUIElement");
        let mut value: CFTypeRef = std::ptr::null();
        let status = unsafe {
            AXUIElementCopyAttributeValue(
                system_wide.0,
                attribute.as_concrete_TypeRef(),
                &mut value,
            )
        };

        match status {
            K_AX_ERROR_SUCCESS if !value.is_null() => {}
            K_AX_ERROR_API_DISABLED => return Err(ApiDisabled),
            _ => {
                if !value.is_null() {
                    unsafe { CFRelease(value) };
                }
                return Ok(None);
            }
        }

        // Refuse to reinterpret the pointer unless CoreFoundation agrees it is
        // an AXUIElement.
        if unsafe { CFGetTypeID(value) } != unsafe { AXUIElementGetTypeID() } {
            unsafe { CFRelease(value) };
            return Ok(None);
        }

        let element = AxElement(value as AXUIElementRef);
        let role = copy_string_attribute(&element, "AXRole");
        let subrole = copy_string_attribute(&element, "AXSubrole");

        Ok(Some(Self {
            element,
            role,
            subrole,
        }))
    }

    /// True when the focused control is a password/secure field. Callers must
    /// refuse the read entirely — including the clipboard fallback.
    pub fn is_secure(&self) -> bool {
        matches!(self.subrole.as_deref(), Some("AXSecureTextField"))
    }

    /// `Safe` only for controls we positively recognise as plain text; anything
    /// else is `Unknown` and stays off cloud backends.
    ///
    /// `AXWebArea` is deliberately **not** safe. A web area is a whole document:
    /// its selection can span a password input, and the element-level
    /// `AXSecureTextField` check above only sees the focused control. Treating
    /// a page as plain text would let that reach a cloud provider.
    pub fn sensitivity(&self) -> Sensitivity {
        const SAFE_ROLES: [&str; 5] = [
            "AXTextField",
            "AXTextArea",
            "AXStaticText",
            "AXComboBox",
            "AXSearchField",
        ];

        match self.role.as_deref() {
            Some(role) if SAFE_ROLES.contains(&role) => Sensitivity::Safe,
            _ => Sensitivity::Unknown,
        }
    }

    pub fn role(&self) -> Option<&str> {
        self.role.as_deref()
    }

    pub fn selected_text(&self) -> AttributeRead {
        read_string_attribute(&self.element, "AXSelectedText")
    }
}

pub struct ApiDisabled;

fn copy_string_attribute(element: &AxElement, attribute: &str) -> Option<String> {
    match read_string_attribute(element, attribute) {
        AttributeRead::Text(text) => Some(text),
        _ => None,
    }
}

fn read_string_attribute(element: &AxElement, attribute: &str) -> AttributeRead {
    let name = CFString::new(attribute);
    let mut value: CFTypeRef = std::ptr::null();
    let status =
        unsafe { AXUIElementCopyAttributeValue(element.0, name.as_concrete_TypeRef(), &mut value) };

    if status != K_AX_ERROR_SUCCESS || value.is_null() {
        if !value.is_null() {
            unsafe { CFRelease(value) };
        }
        return match status {
            K_AX_ERROR_API_DISABLED => AttributeRead::ApiDisabled,
            K_AX_ERROR_NO_VALUE => AttributeRead::Empty,
            K_AX_ERROR_ATTRIBUTE_UNSUPPORTED | K_AX_ERROR_NOT_IMPLEMENTED => {
                AttributeRead::Unsupported
            }
            // Anything else (invalid element, cannot complete, timeout) means we
            // learned nothing about this control, so let the next layer try.
            _ => AttributeRead::Unsupported,
        };
    }

    if unsafe { CFGetTypeID(value) } != CFString::type_id() {
        unsafe { CFRelease(value) };
        return AttributeRead::Unsupported;
    }

    // wrap_under_create_rule takes ownership of the +1 retain from Copy…
    let text = unsafe { CFString::wrap_under_create_rule(value as CFStringRef) }.to_string();
    if text.is_empty() {
        AttributeRead::Empty
    } else {
        AttributeRead::Text(text)
    }
}
