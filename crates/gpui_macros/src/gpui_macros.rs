mod derive_action;
mod derive_app_context;
mod derive_into_element;
mod derive_render;
mod derive_visual_context;
mod register_action;
mod styles;

#[cfg(any(feature = "inspector", debug_assertions))]
mod derive_inspector_reflection;

use proc_macro::TokenStream;
use syn::{DeriveInput, Ident};

/// `Action` derive macro - see the trait documentation for details.
#[proc_macro_derive(Action, attributes(action))]
pub fn derive_action(input: TokenStream) -> TokenStream {
  derive_action::derive_action(input)
}

/// This can be used to register an action with the GPUI runtime when you want to manually implement
/// the `Action` trait. Typically you should use the `Action` derive macro or `actions!` macro
/// instead.
#[proc_macro]
pub fn register_action(ident: TokenStream) -> TokenStream {
  register_action::register_action(ident)
}

/// #[derive(IntoElement)] generates an `IntoElement` impl for any `RenderOnce`
/// type, wrapping it in a `ViewElement` so it can be used as a child.
#[proc_macro_derive(IntoElement)]
pub fn derive_into_element(input: TokenStream) -> TokenStream {
  derive_into_element::derive_into_element(input)
}

#[proc_macro_derive(Render)]
#[doc(hidden)]
pub fn derive_render(input: TokenStream) -> TokenStream {
  derive_render::derive_render(input)
}

/// #[derive(AppContext)] is used to create a context out of anything that holds a `&mut App`
/// Note that a `#[app]` attribute is required to identify the variable holding the &mut App.
///
/// Failure to add the attribute causes a compile error:
///
/// ```compile_fail
/// # #[macro_use] extern crate gpui_macros;
/// # #[macro_use] extern crate gpui;
/// #[derive(AppContext)]
/// struct MyContext<'a> {
///     app: &'a mut gpui::App
/// }
/// ```
#[proc_macro_derive(AppContext, attributes(app))]
pub fn derive_app_context(input: TokenStream) -> TokenStream {
  derive_app_context::derive_app_context(input)
}

/// #[derive(VisualContext)] is used to create a visual context out of anything that holds a `&mut Window` and
/// implements `AppContext`
/// Note that a `#[app]` and a `#[window]` attribute are required to identify the variables holding the &mut App,
/// and &mut Window respectively.
///
/// Failure to add both attributes causes a compile error:
///
/// ```compile_fail
/// # #[macro_use] extern crate gpui_macros;
/// # #[macro_use] extern crate gpui;
/// #[derive(VisualContext)]
/// struct MyContext<'a, 'b> {
///     #[app]
///     app: &'a mut gpui::App,
///     window: &'b mut gpui::Window
/// }
/// ```
///
/// ```compile_fail
/// # #[macro_use] extern crate gpui_macros;
/// # #[macro_use] extern crate gpui;
/// #[derive(VisualContext)]
/// struct MyContext<'a, 'b> {
///     app: &'a mut gpui::App,
///     #[window]
///     window: &'b mut gpui::Window
/// }
/// ```
#[proc_macro_derive(VisualContext, attributes(window, app))]
pub fn derive_visual_context(input: TokenStream) -> TokenStream {
  derive_visual_context::derive_visual_context(input)
}

/// Used by GPUI to generate the style helpers.
#[proc_macro]
#[doc(hidden)]
pub fn style_helpers(input: TokenStream) -> TokenStream {
  styles::style_helpers(input)
}

/// Generates methods for visibility styles.
#[proc_macro]
pub fn visibility_style_methods(input: TokenStream) -> TokenStream {
  styles::visibility_style_methods(input)
}

/// Generates methods for margin styles.
#[proc_macro]
pub fn margin_style_methods(input: TokenStream) -> TokenStream {
  styles::margin_style_methods(input)
}

/// Generates methods for padding styles.
#[proc_macro]
pub fn padding_style_methods(input: TokenStream) -> TokenStream {
  styles::padding_style_methods(input)
}

/// Generates methods for position styles.
#[proc_macro]
pub fn position_style_methods(input: TokenStream) -> TokenStream {
  styles::position_style_methods(input)
}

/// Generates methods for overflow styles.
#[proc_macro]
pub fn overflow_style_methods(input: TokenStream) -> TokenStream {
  styles::overflow_style_methods(input)
}

/// Generates methods for cursor styles.
#[proc_macro]
pub fn cursor_style_methods(input: TokenStream) -> TokenStream {
  styles::cursor_style_methods(input)
}

/// Generates methods for border styles.
#[proc_macro]
pub fn border_style_methods(input: TokenStream) -> TokenStream {
  styles::border_style_methods(input)
}

/// Generates methods for box shadow styles.
#[proc_macro]
pub fn box_shadow_style_methods(input: TokenStream) -> TokenStream {
  styles::box_shadow_style_methods(input)
}

/// When added to a trait, `#[derive_inspector_reflection]` generates a module which provides
/// enumeration and lookup by name of all methods that have the shape `fn method(self) -> Self`.
/// This is used by the inspector so that it can use the builder methods in `Styled` and
/// `StyledExt`.
///
/// The generated module will have the name `<snake_case_trait_name>_reflection` and contain the
/// following functions:
///
/// ```ignore
/// pub fn methods::<T: TheTrait + 'static>() -> Vec<gpui::inspector_reflection::FunctionReflection<T>>;
///
/// pub fn find_method::<T: TheTrait + 'static>() -> Option<gpui::inspector_reflection::FunctionReflection<T>>;
/// ```
///
/// The `invoke` method on `FunctionReflection` will run the method. `FunctionReflection` also
/// provides the method's documentation.
#[cfg(any(feature = "inspector", debug_assertions))]
#[proc_macro_attribute]
pub fn derive_inspector_reflection(_args: TokenStream, input: TokenStream) -> TokenStream {
  derive_inspector_reflection::derive_inspector_reflection(_args, input)
}

pub(crate) fn get_simple_attribute_field(ast: &DeriveInput, name: &'static str) -> Option<Ident> {
  match &ast.data {
    syn::Data::Struct(data_struct) => data_struct
      .fields
      .iter()
      .find(|field| field.attrs.iter().any(|attr| attr.path().is_ident(name)))
      .map(|field| field.ident.clone().unwrap()),
    syn::Data::Enum(_) => None,
    syn::Data::Union(_) => None,
  }
}
