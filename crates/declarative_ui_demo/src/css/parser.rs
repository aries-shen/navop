use super::properties::parse_properties;
use super::{
    CssDeclaration, CssError, CssLimits, CssResource, CssRule, CssSelector, CssStylesheet,
};

pub fn parse_css(source: &str, limits: crate::CompileLimits) -> Result<CssStylesheet, CssError> {
    let limits = CssLimits::from(limits);
    enforce_limit(
        CssResource::SourceBytes,
        source.len(),
        limits.max_source_bytes,
    )?;
    let mut parser = CssParser {
        input: source,
        bytes: source.as_bytes(),
        position: 0,
        limits,
        rules: 0,
        selectors: 0,
        declarations: 0,
    };
    let rules = parser.parse_rules()?;
    Ok(CssStylesheet { rules })
}

struct CssParser<'a> {
    input: &'a str,
    bytes: &'a [u8],
    position: usize,
    limits: CssLimits,
    rules: usize,
    selectors: usize,
    declarations: usize,
}

impl CssParser<'_> {
    fn parse_rules(&mut self) -> Result<Vec<CssRule>, CssError> {
        let mut rules = Vec::new();
        while !self.is_eof() {
            self.skip_ignored();
            if self.is_eof() {
                break;
            }
            if self.peek() == Some(b'@') {
                return Err(self.error("at-rules are not supported"));
            }
            rules.push(self.parse_rule()?);
        }
        Ok(rules)
    }

    fn parse_rule(&mut self) -> Result<CssRule, CssError> {
        let selectors = self.parse_selectors()?;
        self.skip_ignored();
        self.expect(b'{', "expected `{` before declarations")?;
        let declarations = self.parse_declarations()?;
        self.skip_ignored();
        self.expect(b'}', "expected `}` after declarations")?;
        self.consume_limit(CssResource::Rules, 1, self.limits.max_rules)?;
        Ok(CssRule {
            selectors,
            declarations,
        })
    }

    fn parse_selectors(&mut self) -> Result<Vec<CssSelector>, CssError> {
        let mut selectors = Vec::new();
        loop {
            self.skip_ignored();
            let start = self.position;
            let selector = self.parse_selector()?;
            self.skip_ignored();
            if selector == CssSelector::default() {
                return Err(self.error_at(start, "selector must not be empty"));
            }
            self.consume_limit(CssResource::Selectors, 1, self.limits.max_selectors)?;
            selectors.push(selector);
            self.skip_ignored();
            if self.peek() == Some(b',') {
                self.position += 1;
            } else {
                return Ok(selectors);
            }
        }
    }

    fn parse_selector(&mut self) -> Result<CssSelector, CssError> {
        let mut selector = CssSelector::default();
        while let Some(byte) = self.peek() {
            match byte {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => {
                    let token = self.take_identifier()?;
                    if selector.tag.is_none()
                        && selector.classes.is_empty()
                        && selector.id.is_none()
                    {
                        selector.tag = Some(token);
                    } else {
                        return Err(self.error("compound selectors may not contain bare tokens"));
                    }
                }
                b'.' => {
                    self.position += 1;
                    selector.classes.push(self.take_identifier()?);
                }
                b'#' => {
                    self.position += 1;
                    let token = self.take_identifier()?;
                    if selector.id.replace(token).is_some() {
                        return Err(self.error("selector contains multiple ids"));
                    }
                }
                b'{' | b',' | b'}' => return Ok(selector),
                byte if byte.is_ascii_whitespace() => return Ok(selector),
                _ => return Err(self.error("unsupported selector syntax")),
            }
        }
        Ok(selector)
    }

    fn parse_declarations(&mut self) -> Result<Vec<CssDeclaration>, CssError> {
        let mut declarations = Vec::new();
        loop {
            self.skip_ignored();
            match self.peek() {
                Some(b'}') => return Ok(declarations),
                None => return Err(self.error("unterminated declaration block")),
                _ => {}
            }
            let name = self.take_identifier()?;
            self.skip_ignored();
            self.expect(b':', "expected `:` after property name")?;
            self.skip_ignored();
            let value_start = self.position;
            let value = self.take_declaration_value()?;
            self.skip_ignored();
            self.expect(b';', "expected `;` after declaration value")?;
            let properties = parse_properties(&name, value.as_str())
                .ok_or_else(|| self.error_at(value_start, "unsupported property or value"))?;
            self.consume_limit(
                CssResource::Declarations,
                properties.len(),
                self.limits.max_declarations,
            )?;
            declarations.extend(
                properties
                    .into_iter()
                    .map(|property| CssDeclaration { property }),
            );
        }
    }
}

impl CssParser<'_> {
    fn is_eof(&self) -> bool {
        self.position >= self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn error(&self, message: &str) -> CssError {
        self.error_at(self.position, message)
    }

    fn error_at(&self, position: usize, message: &str) -> CssError {
        CssError::Syntax {
            position,
            message: message.to_owned(),
        }
    }

    fn expect(&mut self, expected: u8, message: &str) -> Result<(), CssError> {
        if self.peek() == Some(expected) {
            self.position += 1;
            Ok(())
        } else {
            Err(self.error(message))
        }
    }

    fn skip_ignored(&mut self) {
        while let Some(byte) = self.peek() {
            if byte.is_ascii_whitespace() {
                self.position += 1;
            } else if self.input[self.position..].starts_with("/*") {
                self.position += 2;
                self.skip_comment();
            } else {
                break;
            }
        }
    }

    fn skip_comment(&mut self) {
        while self.position < self.bytes.len() {
            if self.input[self.position..].starts_with("*/") {
                self.position += 2;
                return;
            }
            self.position += 1;
        }
    }

    fn take_identifier(&mut self) -> Result<String, CssError> {
        let start = self.position;
        while matches!(
            self.peek(),
            Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_')
        ) {
            self.position += 1;
        }
        if self.position == start {
            return Err(self.error("expected identifier"));
        }
        Ok(self.input[start..self.position].to_owned())
    }

    fn take_declaration_value(&mut self) -> Result<String, CssError> {
        let start = self.position;
        while matches!(self.peek(), Some(byte) if !matches!(byte, b';' | b'}')) {
            self.position += 1;
        }
        if self.position == start {
            return Err(self.error("expected declaration value"));
        }
        Ok(self.input[start..self.position].trim().to_owned())
    }

    fn consume_limit(
        &mut self,
        resource: CssResource,
        increment: usize,
        limit: usize,
    ) -> Result<(), CssError> {
        let current = match resource {
            CssResource::Rules => &mut self.rules,
            CssResource::Selectors => &mut self.selectors,
            CssResource::Declarations => &mut self.declarations,
            CssResource::SourceBytes => return Ok(()),
        };
        *current = current.saturating_add(increment);
        enforce_limit(resource, *current, limit)
    }
}

fn enforce_limit(resource: CssResource, actual: usize, limit: usize) -> Result<(), CssError> {
    if actual > limit {
        return Err(CssError::ResourceLimitExceeded {
            resource,
            limit,
            actual,
        });
    }
    Ok(())
}
