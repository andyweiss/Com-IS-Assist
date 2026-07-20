use glib::prelude::StaticType;
use gstreamer as gst;

mod imp;

glib::wrapper! {
    pub struct ComISAssist(ObjectSubclass<imp::ComISAssist>) @extends gst::Element, gst::Object;
}

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "comisassist",
        gst::Rank::NONE,
        ComISAssist::static_type(),
    )
}

// "GPL" here matches the project-wide GPLv3 decision in docs/LICENSING.md (driven by the VST3
// target's dependency chain, not by this element itself) - GStreamer's plugin registry expects
// one of a small set of recognized license strings, and "GPL" is the conventional value used
// regardless of exact GPL version (matching how other GPL-licensed GStreamer plugins declare it).
gst::plugin_define!(
    comisassist,
    "Com-IS-Assist broadcast automixer elements",
    plugin_init,
    env!("CARGO_PKG_VERSION"),
    "GPL",
    "comisassist",
    "comisassist",
    "https://github.com/andyweiss/Com-IS-Assist"
);
