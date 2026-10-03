//! `pairly-gtk`: libadwaita front end. A pure D-Bus client of `pairlyd`.
#![forbid(unsafe_code)]

use relm4::adw::prelude::*;
use relm4::{ComponentParts, ComponentSender, RelmApp, SimpleComponent, adw};

const APP_ID: &str = "dev.pairly.Pairly";

struct App;

#[relm4::component]
impl SimpleComponent for App {
    type Init = ();
    type Input = ();
    type Output = ();

    view! {
        adw::ApplicationWindow {
            set_title: Some("Pairly"),
            set_default_size: (900, 600),

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &adw::StatusPage {
                    set_icon_name: Some("phone-symbolic"),
                    set_title: "Pairly",
                    set_description: Some("No devices yet. Pairing arrives in Phase 4."),
                },
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        _sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let model = App;
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }
}

fn main() {
    RelmApp::new(APP_ID).run::<App>(());
}
