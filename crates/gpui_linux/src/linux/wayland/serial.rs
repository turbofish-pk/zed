use collections::HashMap;

#[derive(Debug, Hash, PartialEq, Eq)]
pub(crate) enum SerialKind {
    DataDevice,
    InputMethod,
    MouseEnter,
    MousePress,
    KeyPress,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Serial(u32);

impl Serial {
    pub(super) fn as_raw(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SelectionSerial(Serial);

impl SelectionSerial {
    pub(super) fn as_raw(self) -> u32 {
        self.0.as_raw()
    }
}

#[derive(Debug)]
/// Helper for tracking of different serial kinds.
pub(crate) struct SerialTracker {
    serials: HashMap<SerialKind, Serial>,
    selection_serial: Option<SelectionSerial>,
}

impl SerialTracker {
    pub fn new() -> Self {
        Self {
            serials: HashMap::default(),
            selection_serial: None,
        }
    }

    pub fn update(&mut self, kind: SerialKind, value: u32) {
        let serial = Serial(value);

        if matches!(&kind, SerialKind::KeyPress | SerialKind::MousePress) {
            self.selection_serial = Some(SelectionSerial(serial));
        }

        self.serials.insert(kind, serial);
    }

    /// Returns the latest tracked serial of the provided [`SerialKind`].
    ///
    /// Returns a serial with a raw value of 0 if the kind has not been tracked.
    pub fn get(&self, kind: SerialKind) -> Serial {
        self.serials.get(&kind).copied().unwrap_or(Serial(0))
    }

    pub fn selection_serial(&self) -> Option<SelectionSerial> {
        self.selection_serial
    }
}

