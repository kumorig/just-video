//! A small form (text fields) with an on-panel keyboard, drawn into the
//! browser panel and operated by pointing: used to add servers and rename files.

use super::canvas::{Canvas, Fonts, Rgb};

const TEXT: Rgb = [0xe8, 0xea, 0xed];
const SUBTLE: Rgb = [0x9a, 0xa0, 0xa6];
const FIELD: Rgb = [0x1d, 0x21, 0x28];
const FIELD_FOCUS: Rgb = [0x26, 0x2f, 0x40];
const KEY: Rgb = [0x2a, 0x2f, 0x38];
const KEY_HOVER: Rgb = [0x3a, 0x42, 0x50];
const ACCENT: Rgb = [0x4f, 0x8c, 0xff];
const SELECTION: Rgb = [0x2f, 0x4f, 0x80];
const ERROR: Rgb = [0xff, 0x6b, 0x60];

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub label: String,
    pub value: String,
    /// Shown as dots (passwords).
    pub secret: bool,
    pub placeholder: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layer {
    Lower,
    Upper,
    Symbols,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    pub title: String,
    pub fields: Vec<Field>,
    pub focused: usize,
    /// Caret in the focused field, in characters from its start.
    pub cursor: usize,
    /// The other end of the selection, when text is selected.
    pub anchor: Option<usize>,
    pub layer: Layer,
    pub error: Option<String>,
    pub busy: Option<String>,
    pub submit: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Shift,
    Symbols,
    Backspace,
    /// Caret one character left or right.
    Left,
    Right,
    Space,
    /// The selection (or the whole field) to the owner's clipboard, and back.
    Copy,
    Paste,
    Cancel,
    Submit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// A field, and the caret position under the pointer.
    Field(usize, usize),
    Key(Key),
    Nothing,
}

impl Form {
    pub fn new(title: impl Into<String>, fields: Vec<Field>, submit: impl Into<String>) -> Self {
        let cursor = fields.first().map_or(0, |f| f.value.chars().count());
        Self {
            title: title.into(),
            fields,
            focused: 0,
            cursor,
            anchor: None,
            layer: Layer::Lower,
            error: None,
            busy: None,
            submit: submit.into(),
        }
    }

    pub fn value(&self, i: usize) -> &str {
        &self.fields[i].value
    }

    /// Focuses field `i` with the caret at `cursor` (None: at the end).
    pub fn focus(&mut self, i: usize, cursor: Option<usize>) {
        self.focused = i;
        self.anchor = None;
        let len = self.fields[i].value.chars().count();
        self.cursor = cursor.unwrap_or(len).min(len);
    }

    /// The selected characters of the focused field, start to end.
    pub fn selection(&self) -> Option<(usize, usize)> {
        let anchor = self.anchor?;
        (anchor != self.cursor).then(|| (anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    /// Selects characters `anchor` to `cursor` of field `i` (either order).
    pub fn select_range(&mut self, i: usize, anchor: usize, cursor: usize) {
        self.focus(i, Some(cursor));
        let len = self.fields[i].value.chars().count();
        self.anchor = Some(anchor.min(len));
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.fields[self.focused].value.chars().count();
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Removes the selection; false when there was none.
    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else {
            return false;
        };
        let field = &mut self.fields[self.focused];
        let (from, to) = (byte_at(&field.value, a), byte_at(&field.value, b));
        field.value.replace_range(from..to, "");
        self.cursor = a;
        self.anchor = None;
        true
    }

    /// Applies a key; returns Cancel/Submit for the owner to act on. The
    /// clipboard is the owner's, so it outlives the form.
    pub fn press(&mut self, key: Key, clipboard: &mut String) -> Option<Key> {
        self.error = None;
        let len = self.fields[self.focused].value.chars().count();
        self.cursor = self.cursor.min(len);
        match key {
            Key::Char(_) | Key::Space | Key::Paste => {
                let text = match key {
                    Key::Char(c) => c.to_string(),
                    Key::Space => " ".to_string(),
                    _ => clipboard.clone(),
                };
                if text.is_empty() {
                    return None;
                }
                self.delete_selection();
                let field = &mut self.fields[self.focused];
                field
                    .value
                    .insert_str(byte_at(&field.value, self.cursor), &text);
                self.cursor += text.chars().count();
                if matches!(key, Key::Char(_)) && self.layer == Layer::Upper {
                    self.layer = Layer::Lower; // one-shot shift
                }
            }
            Key::Backspace => {
                if !self.delete_selection() && self.cursor > 0 {
                    self.cursor -= 1;
                    let field = &mut self.fields[self.focused];
                    field.value.remove(byte_at(&field.value, self.cursor));
                }
            }
            Key::Left => match self.selection() {
                Some((start, _)) => {
                    self.cursor = start;
                    self.anchor = None;
                }
                None => self.cursor = self.cursor.saturating_sub(1),
            },
            Key::Right => match self.selection() {
                Some((_, end)) => {
                    self.cursor = end;
                    self.anchor = None;
                }
                None => self.cursor = (self.cursor + 1).min(len),
            },
            Key::Copy => {
                let field = &self.fields[self.focused];
                if !field.secret {
                    *clipboard = match self.selection() {
                        Some((a, b)) => field.value.chars().skip(a).take(b - a).collect(),
                        None => field.value.clone(),
                    };
                }
            }
            Key::Shift => {
                self.layer = if self.layer == Layer::Upper {
                    Layer::Lower
                } else {
                    Layer::Upper
                };
            }
            Key::Symbols => {
                self.layer = if self.layer == Layer::Symbols {
                    Layer::Lower
                } else {
                    Layer::Symbols
                };
            }
            Key::Cancel | Key::Submit => return Some(key),
        }
        None
    }
}

/// Byte offset of character `chars` in `value`.
fn byte_at(value: &str, chars: usize) -> usize {
    value
        .char_indices()
        .nth(chars)
        .map_or(value.len(), |(i, _)| i)
}

// Layout inside the 1600×1000 browser panel.
const X0: f32 = 32.0;
const FIELD_Y: f32 = 132.0;
const FIELD_H: f32 = 72.0;
const FIELD_GAP: f32 = 10.0;
const KEY_H: f32 = 84.0;
const KEY_GAP: f32 = 10.0;

fn rows(layer: Layer) -> [&'static str; 4] {
    match layer {
        Layer::Lower => ["1234567890", "qwertyuiop", "asdfghjkl@", "zxcvbnm.-_"],
        Layer::Upper => ["1234567890", "QWERTYUIOP", "ASDFGHJKL@", "ZXCVBNM.-_"],
        Layer::Symbols => ["!#$%&*()+=", "\\/:;,'\"?~^", "[]{}<>|`€£", "§°¨´¤½.-_ "],
    }
}

pub type Rect = (f32, f32, f32, f32);

fn keyboard_top(form: &Form) -> f32 {
    FIELD_Y + form.fields.len() as f32 * (FIELD_H + FIELD_GAP) + 24.0
}

fn field_rect(i: usize) -> Rect {
    (
        X0 + 260.0,
        FIELD_Y + i as f32 * (FIELD_H + FIELD_GAP),
        1600.0 - 2.0 * X0 - 260.0,
        FIELD_H,
    )
}

/// Every key with its rectangle.
fn keys(form: &Form, panel_width: f32) -> Vec<(Key, Rect)> {
    let top = keyboard_top(form);
    let width = panel_width - 2.0 * X0;
    let mut out = Vec::new();
    // Four character rows of 10 keys; the wide keys sit on the sides.
    for (r, chars) in rows(form.layer).iter().enumerate() {
        let y = top + r as f32 * (KEY_H + KEY_GAP);
        let side = 180.0;
        let key_w = (width - 2.0 * (side + KEY_GAP) - 9.0 * KEY_GAP) / 10.0;
        let (left, right) = match r {
            0 => (None, Some(Key::Backspace)),
            1 => (Some(Key::Left), Some(Key::Right)),
            2 => (Some(Key::Shift), Some(Key::Copy)),
            3 => (Some(Key::Symbols), Some(Key::Paste)),
            _ => (None, None),
        };
        if let Some(k) = left {
            out.push((k, (X0, y, side, KEY_H)));
        }
        for (i, c) in chars.chars().enumerate() {
            let x = X0 + side + KEY_GAP + i as f32 * (key_w + KEY_GAP);
            out.push((
                if c == ' ' { Key::Space } else { Key::Char(c) },
                (x, y, key_w, KEY_H),
            ));
        }
        if let Some(k) = right {
            out.push((k, (X0 + width - side, y, side, KEY_H)));
        }
    }
    // Bottom row: cancel, space, submit.
    let y = top + 4.0 * (KEY_H + KEY_GAP);
    out.push((Key::Cancel, (X0, y, 300.0, KEY_H)));
    out.push((Key::Space, (X0 + 310.0, y, width - 620.0, KEY_H)));
    out.push((Key::Submit, (X0 + width - 300.0, y, 300.0, KEY_H)));
    out
}

fn inside((x, y, w, h): Rect, px: f32, py: f32) -> bool {
    px >= x && px <= x + w && py >= y && py <= y + h
}

/// The text shown in a field (dots for secrets).
fn shown(field: &Field) -> Vec<char> {
    if field.secret {
        vec!['•'; field.value.chars().count()]
    } else {
        field.value.chars().collect()
    }
}

const TEXT_SIZE: f32 = 34.0;

/// First character shown in field `i`: from the start if it all fits,
/// otherwise so the end is visible, or, if the caret is further left,
/// from the caret. Clicking inside the shown text never scrolls it.
fn first_shown(form: &Form, i: usize, fonts: &mut Fonts) -> usize {
    let chars = shown(&form.fields[i]);
    let room = field_rect(i).2 - 60.0;
    let mut start = 0;
    while start + 1 < chars.len()
        && fonts.measure(&chars[start..].iter().collect::<String>(), TEXT_SIZE) > room
    {
        start += 1;
    }
    if i == form.focused {
        start.min(form.cursor)
    } else {
        start
    }
}

/// The caret position nearest to canvas x in field `i`.
fn caret_at(form: &Form, i: usize, x: f32, fonts: &mut Fonts) -> usize {
    let chars = shown(&form.fields[i]);
    let start = first_shown(form, i, fonts);
    let mut left = field_rect(i).0 + 20.0;
    for (k, c) in chars.iter().enumerate().skip(start) {
        let w = fonts.measure(&c.to_string(), TEXT_SIZE);
        if x < left + w / 2.0 {
            return k;
        }
        left += w;
    }
    chars.len()
}

/// What the D-pad moves between: the fields (caret at the end) and the keys.
pub fn targets(form: &Form, panel_width: f32) -> Vec<(Hit, Rect)> {
    if form.busy.is_some() {
        return Vec::new();
    }
    let fields = form
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| (Hit::Field(i, f.value.chars().count()), field_rect(i)));
    let keys = keys(form, panel_width)
        .into_iter()
        .map(|(k, r)| (Hit::Key(k), r));
    fields.chain(keys).collect()
}

pub fn hit(form: &Form, fonts: &mut Fonts, panel_width: f32, x: f32, y: f32) -> Hit {
    if form.busy.is_some() {
        return Hit::Nothing;
    }
    for i in 0..form.fields.len() {
        if inside(field_rect(i), x, y) {
            return Hit::Field(i, caret_at(form, i, x, fonts));
        }
    }
    keys(form, panel_width)
        .into_iter()
        .find(|(_, r)| inside(*r, x, y))
        .map_or(Hit::Nothing, |(k, _)| Hit::Key(k))
}

fn key_label(key: Key, form: &Form) -> String {
    match key {
        Key::Char(c) => c.to_string(),
        Key::Shift => {
            if form.layer == Layer::Upper {
                "SHIFT".into()
            } else {
                "Shift".into()
            }
        }
        Key::Symbols => {
            if form.layer == Layer::Symbols {
                "abc".into()
            } else {
                "#+=".into()
            }
        }
        Key::Backspace => "Backspace".into(),
        Key::Copy => "Copy".into(),
        Key::Paste => "Paste".into(),
        // Drawn as triangles.
        Key::Left | Key::Right => String::new(),
        Key::Space => "space".into(),
        Key::Cancel => "Cancel".into(),
        Key::Submit => form.submit.clone(),
    }
}

pub fn render(canvas: &mut Canvas, fonts: &mut Fonts, form: &Form, hover: Hit) {
    let w = canvas.width as f32;
    for (i, field) in form.fields.iter().enumerate() {
        let (fx, fy, fw, fh) = field_rect(i);
        fonts.draw(canvas, &field.label, X0, fy + 47.0, 30.0, SUBTLE, 250.0);
        let focused = i == form.focused;
        let hovered = matches!(hover, Hit::Field(f, _) if f == i);
        canvas.rect(
            fx,
            fy,
            fw,
            fh,
            12.0,
            if focused {
                FIELD_FOCUS
            } else if hovered {
                KEY_HOVER
            } else {
                FIELD
            },
        );
        if focused {
            canvas.rect(fx, fy + fh - 4.0, fw, 4.0, 2.0, ACCENT);
        }
        let chars = shown(field);
        if chars.is_empty() {
            fonts.draw(
                canvas,
                &field.placeholder,
                fx + 20.0,
                fy + 48.0,
                32.0,
                [0x5f, 0x63, 0x68],
                fw - 40.0,
            );
        }
        let start = first_shown(form, i, fonts);
        if focused && let Some((a, b)) = form.selection() {
            // The selected stretch, within what is shown.
            let mut x_of = |k: usize| {
                let before: String = chars[start..k.max(start)].iter().collect();
                (fx + 20.0 + fonts.measure(&before, TEXT_SIZE)).min(fx + fw - 20.0)
            };
            let (x0, x1) = (x_of(a), x_of(b));
            if x1 > x0 {
                canvas.rect(x0, fy + 14.0, x1 - x0, fh - 28.0, 4.0, SELECTION);
            }
        }
        let text: String = chars[start..].iter().collect();
        fonts.draw(
            canvas,
            &text,
            fx + 20.0,
            fy + 48.0,
            TEXT_SIZE,
            TEXT,
            fw - 40.0,
        );
        if focused {
            let before: String = chars[start..form.cursor.clamp(start, chars.len())]
                .iter()
                .collect();
            let x = fx + 20.0 + fonts.measure(&before, TEXT_SIZE);
            canvas.rect(x, fy + 16.0, 3.0, fh - 32.0, 1.0, ACCENT);
        }
    }
    let status_y = keyboard_top(form) - 6.0;
    if let Some(busy) = &form.busy {
        fonts.draw(canvas, busy, X0, status_y, 26.0, SUBTLE, w - 2.0 * X0);
    } else if let Some(error) = &form.error {
        fonts.draw(canvas, error, X0, status_y, 26.0, ERROR, w - 2.0 * X0);
    }
    for (key, (x, y, kw, kh)) in keys(form, w) {
        let hovered = hover == Hit::Key(key);
        let color = match key {
            Key::Submit => {
                if hovered {
                    [0x6b, 0xa0, 0xff]
                } else {
                    ACCENT
                }
            }
            _ if hovered => KEY_HOVER,
            _ => KEY,
        };
        canvas.rect(x, y, kw, kh, 12.0, color);
        let label = key_label(key, form);
        let size = if matches!(key, Key::Char(_)) {
            40.0
        } else {
            32.0
        };
        if matches!(key, Key::Left | Key::Right) {
            // A triangle from vertical strips, pointing its way.
            let (cx, cy) = (x + kw / 2.0, y + kh / 2.0);
            for i in 0..28 {
                let t = i as f32;
                let half = 16.0 * t / 28.0;
                let px = if key == Key::Left {
                    cx - 14.0 + t
                } else {
                    cx + 14.0 - t
                };
                canvas.rect(px, cy - half, 1.5, half * 2.0, 0.0, TEXT);
            }
        }
        let lw = fonts.measure(&label, size);
        fonts.draw(
            canvas,
            &label,
            x + (kw - lw) / 2.0,
            y + kh / 2.0 + size * 0.36,
            size,
            TEXT,
            kw,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> Form {
        Form::new(
            "Add server",
            vec![
                Field {
                    label: "Address".into(),
                    value: String::new(),
                    secret: false,
                    placeholder: String::new(),
                },
                Field {
                    label: "Password".into(),
                    value: String::new(),
                    secret: true,
                    placeholder: String::new(),
                },
            ],
            "Save",
        )
    }

    #[test]
    fn typing_shift_and_backspace() {
        let mut f = form();
        let mut clip = String::new();
        f.press(Key::Shift, &mut clip);
        f.press(Key::Char('N'), &mut clip);
        f.press(Key::Char('a'), &mut clip);
        f.press(Key::Char('s'), &mut clip);
        f.press(Key::Backspace, &mut clip);
        assert_eq!(f.value(0), "Na");
        assert_eq!(f.layer, Layer::Lower, "shift is one-shot");
        assert_eq!(f.press(Key::Submit, &mut clip), Some(Key::Submit));
    }

    #[test]
    fn editing_in_the_middle() {
        let mut f = form();
        let mut clip = String::new();
        f.fields[0].value = "clip.mp4".into();
        f.focus(0, Some(4));
        f.press(Key::Char('2'), &mut clip);
        assert_eq!(f.value(0), "clip2.mp4");
        f.press(Key::Left, &mut clip);
        f.press(Key::Backspace, &mut clip);
        assert_eq!(f.value(0), "cli2.mp4");
        assert_eq!(f.cursor, 3);
        f.press(Key::Right, &mut clip);
        f.press(Key::Right, &mut clip);
        f.press(Key::Space, &mut clip);
        assert_eq!(f.value(0), "cli2. mp4");
        f.focus(0, Some(99));
        assert_eq!(f.cursor, 9, "clamped to the end");
        f.fields[0].value = "åäö".into();
        f.focus(0, Some(1));
        f.press(Key::Backspace, &mut clip);
        assert_eq!(f.value(0), "äö", "characters, not bytes");
    }

    #[test]
    fn selection_is_replaced_or_deleted() {
        let mut f = form();
        let mut clip = String::new();
        f.fields[0].value = "clip.mp4".into();
        f.select_range(0, 4, 0);
        assert_eq!(f.selection(), Some((0, 4)), "either order");
        f.press(Key::Char('x'), &mut clip);
        assert_eq!(f.value(0), "x.mp4");
        assert_eq!((f.cursor, f.selection()), (1, None));
        f.fields[0].value = "clip.mp4".into();
        f.select_range(0, 2, 6);
        f.press(Key::Backspace, &mut clip);
        assert_eq!(f.value(0), "clp4");
        assert_eq!(f.cursor, 2);
        f.select_range(0, 1, 3);
        f.press(Key::Left, &mut clip);
        assert_eq!(
            (f.cursor, f.selection()),
            (1, None),
            "collapses to the start"
        );
        f.select_range(0, 1, 3);
        f.press(Key::Right, &mut clip);
        assert_eq!((f.cursor, f.selection()), (3, None), "collapses to the end");
        f.select_all();
        assert_eq!(f.selection(), Some((0, 4)));
        f.focus(0, Some(1));
        assert_eq!(f.selection(), None, "clicking clears it");
    }

    #[test]
    fn copy_and_paste_round_trip() {
        let mut f = form();
        let mut clip = String::new();
        f.fields[0].value = "clip.mp4".into();
        f.select_range(0, 0, 4);
        f.press(Key::Copy, &mut clip);
        assert_eq!(clip, "clip");
        assert_eq!(f.selection(), Some((0, 4)), "copying keeps the selection");
        f.focus(0, None);
        f.press(Key::Paste, &mut clip);
        assert_eq!(f.value(0), "clip.mp4clip");
        assert_eq!(f.cursor, 12);
        f.focus(0, None);
        f.press(Key::Copy, &mut clip);
        assert_eq!(clip, "clip.mp4clip", "nothing selected: the whole field");
        f.fields[1].value = "secret".into();
        f.focus(1, None);
        f.press(Key::Copy, &mut clip);
        assert_eq!(clip, "clip.mp4clip", "passwords are never copied");
        f.select_range(0, 4, 12);
        f.press(Key::Paste, &mut clip);
        assert_eq!(
            f.value(0),
            "clipclip.mp4clip",
            "pasting replaces the selection"
        );
        clip.clear();
        f.select_range(0, 0, 4);
        f.press(Key::Paste, &mut clip);
        assert_eq!(
            f.selection(),
            Some((0, 4)),
            "an empty clipboard does nothing"
        );
    }

    #[test]
    fn every_key_is_hittable_and_distinct() {
        let f = form();
        let mut fonts = Fonts::load().expect("fonts");
        for (key, (x, y, w, h)) in keys(&f, 1600.0) {
            assert_eq!(
                hit(&f, &mut fonts, 1600.0, x + w / 2.0, y + h / 2.0),
                Hit::Key(key),
                "{key:?}"
            );
            assert!(y + h <= 1000.0, "{key:?} below the panel");
            if matches!(key, Key::Copy | Key::Paste) {
                assert_eq!(
                    x,
                    X0 + 1600.0 - 2.0 * X0 - 180.0,
                    "{key:?} at the right edge"
                );
            }
        }
    }
}
