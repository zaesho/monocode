/// Collect authoritative provider parts in their first observed order.
#[derive(Default)]
pub struct MessageParts(Vec<(String, String)>);

impl MessageParts {
    pub fn update(&mut self, id: &str, text: &str) -> String {
        if let Some(part) = self.0.iter_mut().find(|part| part.0 == id) {
            part.1 = text.into();
        } else {
            self.0.push((id.into(), text.into()));
        }
        self.0.iter().map(|part| part.1.as_str()).collect()
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrections_replace_the_original_part_after_later_parts() {
        let mut parts = MessageParts::default();
        assert_eq!(parts.update("a", "Hello worle"), "Hello worle");
        assert_eq!(parts.update("b", "!"), "Hello worle!");
        assert_eq!(parts.update("a", "Hi"), "Hi!");
        parts.clear();
        assert_eq!(parts.update("b", "New"), "New");
    }
}
