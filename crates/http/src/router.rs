pub struct Router {
    routes: Vec<(String, String)>,
}

impl Router {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    pub fn add_route(&mut self, pattern: &str, handler: &str) {
        self.routes.push((pattern.to_string(), handler.to_string()));
    }

    pub fn match_route(&self, path: &str) -> Option<&str> {
        for (pattern, handler) in &self.routes {
            if path == pattern || path.starts_with(pattern) {
                return Some(handler);
            }
        }
        None
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}
