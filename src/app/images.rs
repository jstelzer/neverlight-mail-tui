use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocolType;
use ratatui_image::thread::ResizeRequest;

use super::App;

impl App {
    pub fn set_picker(&mut self, picker: Picker) {
        self.picker_protocol = Some(picker.protocol_type());
        self.picker = Some(picker);
    }

    fn protocol_type_name(protocol_type: ProtocolType) -> &'static str {
        match protocol_type {
            ProtocolType::Halfblocks => "halfblocks",
            ProtocolType::Sixel => "sixel",
            ProtocolType::Kitty => "kitty",
            ProtocolType::Iterm2 => "iterm2",
        }
    }

    fn stateful_protocol_name(protocol_type: &StatefulProtocolType) -> &'static str {
        match protocol_type {
            StatefulProtocolType::Halfblocks(_) => "halfblocks",
            StatefulProtocolType::Sixel(_) => "sixel",
            StatefulProtocolType::Kitty(_) => "kitty",
            StatefulProtocolType::ITerm2(_) => "iterm2",
        }
    }

    pub fn image_protocol_label(&self) -> String {
        let picker = self
            .picker_protocol
            .map(Self::protocol_type_name)
            .unwrap_or("none");
        let render = match self.image_protos.get(self.image_index) {
            Some(proto) => proto
                .protocol_type()
                .map(Self::stateful_protocol_name)
                .unwrap_or("pending"),
            None => "none",
        };
        format!("picker:{picker} render:{render}")
    }

    /// Apply a completed image resize (from ThreadProtocol background work).
    pub fn apply_image_resize(&mut self, request: ResizeRequest) {
        if let Ok(resized) = request.resize_encode() {
            if let Some(proto) = self.image_protos.get_mut(self.image_index) {
                proto.update_resized_protocol(resized);
            }
        }
    }
}
