//! ID generation for issues.
//!
//! Implements classic bd ID format: `<prefix>-<hash>` where hash is
//! base36 lowercase (0-9, a-z) with adaptive length based on DB size.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_ID_PREFIX_LEN: usize = 64;
pub const MAX_ID_HASH_LEN: usize = 40;
pub const MAX_ID_LENGTH: usize = MAX_ID_PREFIX_LEN + 1 + MAX_ID_HASH_LEN;
const FALLBACK_HASH_LENGTH: usize = 12;
const MAX_FALLBACK_NONCE: u32 = 2000;
const DEFAULT_ID_TEMPLATE: &str = "{prefix}-{hash}";

/// Positive, repository-local shorthand attached to an issue ID.
///
/// The full issue ID remains authoritative. This type keeps sequence values
/// positive at every in-memory boundary while serializing as a JSON integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct IssueSequenceNumber(i64);

impl IssueSequenceNumber {
    /// Construct a validated issue sequence number.
    ///
    /// # Errors
    ///
    /// Returns a validation error when `value` is not positive.
    pub fn new(value: i64) -> Result<Self> {
        Self::try_from(value).map_err(|message| BeadsError::validation("sequence_number", message))
    }

    /// Return the SQLite/JSON integer representation.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }

    /// Return the next counter value after this assigned sequence.
    ///
    /// # Errors
    ///
    /// Returns an error when the sequence counter is exhausted.
    pub fn checked_successor(self) -> Result<Self> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or_else(|| BeadsError::Config("issue sequence counter exhausted".to_string()))
    }
}

impl TryFrom<i64> for IssueSequenceNumber {
    type Error = &'static str;

    fn try_from(value: i64) -> std::result::Result<Self, Self::Error> {
        if value > 0 {
            Ok(Self(value))
        } else {
            Err("expected positive integer")
        }
    }
}

impl From<IssueSequenceNumber> for i64 {
    fn from(value: IssueSequenceNumber) -> Self {
        value.get()
    }
}

/// Resolve an exact issue ID or a positive numeric sequence shorthand.
///
/// Exact IDs always win, including IDs made entirely of digits. Callers retain
/// control of legacy/hash fallback by handling `None` after this shared policy
/// boundary.
///
/// # Errors
///
/// Returns an error when the existence lookup fails, a numeric shorthand is
/// not a positive sequence number, the sequence lookup fails, or the shorthand
/// matches more than one full ID.
pub fn resolve_exact_or_sequence_fallible<FExists, FSequence>(
    input: &str,
    exact_candidate: &str,
    mut exists: FExists,
    mut find_sequence_ids: FSequence,
) -> Result<Option<String>>
where
    FExists: FnMut(&str) -> Result<bool>,
    FSequence: FnMut(IssueSequenceNumber) -> Result<Vec<String>>,
{
    if !exact_candidate.is_empty() && exists(exact_candidate)? {
        return Ok(Some(exact_candidate.to_string()));
    }

    let trimmed = input.trim();
    if trimmed.is_empty() || !trimmed.chars().all(|character| character.is_ascii_digit()) {
        return Ok(None);
    }

    let raw_sequence = trimmed.parse::<i64>().map_err(|_| {
        BeadsError::validation("id", format!("invalid sequence number '{trimmed}'"))
    })?;
    let sequence = IssueSequenceNumber::new(raw_sequence)?;
    match find_sequence_ids(sequence)?.as_slice() {
        [id] => Ok(Some(id.clone())),
        [] => Ok(None),
        matches => Err(BeadsError::AmbiguousId {
            partial: input.to_string(),
            matches: matches.to_vec(),
        }),
    }
}

/// Default ID generation configuration.
#[derive(Debug, Clone)]
pub struct IdConfig {
    /// Issue ID prefix (e.g., "bd", "`beads_rust`").
    pub prefix: String,
    /// Minimum hash length.
    pub min_hash_length: usize,
    /// Maximum hash length.
    pub max_hash_length: usize,
    /// Maximum collision probability before increasing length.
    pub max_collision_prob: f64,
    /// Optional templated ID generation mode. When disabled, the historical
    /// `<prefix>-<hash>` and `<prefix>-<slug>-<hash>` formats are preserved.
    pub generation: IdGenerationConfig,
}

impl Default for IdConfig {
    fn default() -> Self {
        Self {
            prefix: "br".to_string(),
            min_hash_length: 3,
            max_hash_length: 8,
            max_collision_prob: 0.25,
            generation: IdGenerationConfig::default(),
        }
    }
}

/// Issue ID generation strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdGenerationConfig {
    pub mode: IdGenerationMode,
    pub template: String,
    pub require_slug: bool,
    pub allow_non_unique_template: bool,
}

impl Default for IdGenerationConfig {
    fn default() -> Self {
        Self {
            mode: IdGenerationMode::Generated,
            template: DEFAULT_ID_TEMPLATE.to_string(),
            require_slug: false,
            allow_non_unique_template: false,
        }
    }
}

/// Whether new issue IDs use the historical generator or an explicit template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdGenerationMode {
    Generated,
    Templated,
}

impl IdGenerationMode {
    /// Parse a config value.
    ///
    /// # Errors
    ///
    /// Returns a validation error for unsupported modes.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "generated" | "default" => Ok(Self::Generated),
            "templated" | "template" => Ok(Self::Templated),
            other => Err(BeadsError::validation(
                "id_generation.mode",
                format!("unsupported ID generation mode '{other}'"),
            )),
        }
    }
}

impl IdConfig {
    /// Create a new ID config with the given prefix.
    ///
    /// # Errors
    ///
    /// Returns a configuration error if the prefix contains no usable
    /// characters after normalization.
    pub fn with_prefix(prefix: impl Into<String>) -> Result<Self> {
        Ok(Self {
            prefix: normalize_configured_prefix(&prefix.into())?,
            ..Default::default()
        })
    }
}

/// ID generator that produces unique issue IDs.
#[derive(Debug, Clone)]
pub struct IdGenerator {
    config: IdConfig,
}

/// Issue content used to derive a stable, content-addressed issue ID.
#[derive(Debug, Clone, Copy)]
pub struct IdGenerationInput<'a> {
    /// Issue title.
    pub title: &'a str,
    /// Optional issue description/body.
    pub description: Option<&'a str>,
    /// Optional actor used as the issue creator.
    pub creator: Option<&'a str>,
    /// Creation timestamp included in the hash seed.
    pub created_at: DateTime<Utc>,
    /// Current issue count, used to choose an adaptive hash length.
    pub issue_count: usize,
}

impl IdGenerator {
    /// Create a new ID generator with the given config.
    #[must_use]
    pub const fn new(config: IdConfig) -> Self {
        Self { config }
    }

    /// Create a new ID generator with default config.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(IdConfig::default())
    }

    /// Get the configured prefix.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.config.prefix
    }

    /// Compute the optimal hash length for a given issue count.
    ///
    /// Uses birthday problem approximation to estimate collision probability.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn optimal_length(&self, issue_count: usize) -> usize {
        let n = issue_count as f64;
        let max_prob = self.config.max_collision_prob;

        for len in self.config.min_hash_length..=self.config.max_hash_length {
            // Base36 has 36^len possible values
            let space = 36_f64.powi(len as i32);
            // Birthday problem: P(collision) ≈ 1 - e^(-n²/2d)
            let prob = 1.0 - (-n * n / (2.0 * space)).exp();
            if prob < max_prob {
                return len;
            }
        }
        self.config.max_hash_length
    }

    /// Generate a candidate ID with the given parameters.
    #[must_use]
    pub fn generate_candidate(
        &self,
        title: &str,
        description: Option<&str>,
        creator: Option<&str>,
        created_at: DateTime<Utc>,
        nonce: u32,
        hash_length: usize,
    ) -> String {
        let seed = generate_id_seed(title, description, creator, created_at, nonce);
        let hash_str = compute_id_hash(&seed, hash_length);
        format!("{}-{hash_str}", self.config.prefix)
    }

    /// Generate an ID with a user-supplied slug embedded between the prefix
    /// and the uniquifying hash, e.g. `br-survey-my-thing-8cda`.
    ///
    /// The slug is normalized via [`normalize_slug`] (lowercase ASCII
    /// alphanumerics and single hyphens, capped at 48 chars). If the input
    /// slug normalizes to an empty string, this falls back to the regular
    /// hash-only ID generator. The hash suffix is always appended to keep
    /// IDs unique even when two issues use the same slug.
    ///
    /// # Errors
    ///
    /// Returns the collision checker's error, or `IdCollision` if neither the
    /// slug-shaped nor hash-only bounded search finds a free ID.
    pub fn generate_with_slug<F>(
        &self,
        input: IdGenerationInput<'_>,
        slug: &str,
        mut exists: F,
    ) -> Result<String>
    where
        F: FnMut(&str) -> Result<bool>,
    {
        let normalized = normalize_slug_for_prefix(slug, &self.config.prefix);
        if normalized.is_empty() {
            return self.generate(
                input.title,
                input.description,
                input.creator,
                input.created_at,
                input.issue_count,
                &mut exists,
            );
        }

        let mut length = self.optimal_length(input.issue_count);
        // Cap how much we extend the hash before we fall back to the
        // hash-only path; max_hash_length is bounded by parser limits.
        loop {
            for nonce in 0..10 {
                let seed = generate_id_seed(
                    input.title,
                    input.description,
                    input.creator,
                    input.created_at,
                    nonce,
                );
                let hash_str = compute_id_hash(&seed, length);
                let id = format!("{}-{normalized}-{hash_str}", self.config.prefix);
                if !exists(&id)? {
                    return Ok(id);
                }
            }
            if length < self.config.max_hash_length {
                length += 1;
            } else {
                // Saturated the hash length budget for the slug path. Drop
                // the slug (preserves uniqueness via the standard fallback)
                // rather than producing an oversized prefix.
                return self.generate(
                    input.title,
                    input.description,
                    input.creator,
                    input.created_at,
                    input.issue_count,
                    &mut exists,
                );
            }
        }
    }

    /// Generate an ID from a configured template.
    ///
    /// Supported tokens are `{prefix}`, `{seq}`, `{seq:03}`, `{slug}`, and
    /// `{hash}`. Unknown tokens fail closed. `{hash}` is required unless the
    /// caller explicitly allowed a non-unique template.
    ///
    /// # Errors
    ///
    /// Returns validation errors for malformed templates, missing required
    /// slug values, or exhausted collision attempts.
    #[allow(clippy::literal_string_with_formatting_args)]
    pub fn generate_with_template<F>(
        &self,
        input: IdGenerationInput<'_>,
        sequence: Option<IssueSequenceNumber>,
        slug: Option<&str>,
        mut exists: F,
    ) -> Result<String>
    where
        F: FnMut(&str) -> Result<bool>,
    {
        self.validate_template_config()?;
        let normalized_slug = slug.map(normalize_slug).filter(|value| !value.is_empty());
        if self.config.generation.require_slug && normalized_slug.is_none() {
            return Err(BeadsError::validation(
                "slug",
                "id_generation.require_slug is true but no slug or title-derived slug was available",
            ));
        }
        if self.config.generation.template.contains("{seq") && sequence.is_none() {
            return Err(BeadsError::validation(
                "id_generation.template",
                "template uses {seq} but no sequence was allocated",
            ));
        }

        let mut length = self.optimal_length(input.issue_count);
        loop {
            for nonce in 0..10 {
                let seed = generate_id_seed(
                    input.title,
                    input.description,
                    input.creator,
                    input.created_at,
                    nonce,
                );
                let hash = compute_id_hash(&seed, length);
                let id = render_id_template(
                    &self.config.generation.template,
                    &self.config.prefix,
                    sequence,
                    normalized_slug.as_deref(),
                    &hash,
                )?;
                if !exists(&id)? {
                    return Ok(id);
                }
            }
            if length < self.config.max_hash_length {
                length += 1;
            } else if self.config.generation.template.contains("{hash}") {
                for nonce in 0..MAX_FALLBACK_NONCE {
                    let seed = generate_id_seed(
                        input.title,
                        input.description,
                        input.creator,
                        input.created_at,
                        nonce,
                    );
                    let hash = compute_id_hash(&seed, FALLBACK_HASH_LENGTH);
                    let id = render_id_template(
                        &self.config.generation.template,
                        &self.config.prefix,
                        sequence,
                        normalized_slug.as_deref(),
                        &hash,
                    )?;
                    if !exists(&id)? {
                        return Ok(id);
                    }
                }
                return Err(BeadsError::IdCollision {
                    id: render_id_template(
                        &self.config.generation.template,
                        &self.config.prefix,
                        sequence,
                        normalized_slug.as_deref(),
                        &compute_id_hash(
                            &generate_id_seed(
                                input.title,
                                input.description,
                                input.creator,
                                input.created_at,
                                MAX_FALLBACK_NONCE,
                            ),
                            FALLBACK_HASH_LENGTH,
                        ),
                    )?,
                });
            } else {
                let id = render_id_template(
                    &self.config.generation.template,
                    &self.config.prefix,
                    sequence,
                    normalized_slug.as_deref(),
                    "",
                )?;
                return if exists(&id)? {
                    Err(BeadsError::IdCollision { id })
                } else {
                    Ok(id)
                };
            }
        }
    }

    /// Validate the configured template generation settings without rendering an ID.
    ///
    /// # Errors
    ///
    /// Returns validation errors for unsupported template tokens or unsafe
    /// missing hash suffixes.
    #[allow(clippy::literal_string_with_formatting_args)]
    pub fn validate_template_config(&self) -> Result<()> {
        validate_id_template(&self.config.generation)?;
        let sequence = self
            .config
            .generation
            .template
            .contains("{seq")
            .then_some(IssueSequenceNumber::new(1)?);
        let slug = self
            .config
            .generation
            .template
            .contains("{slug}")
            .then_some("slug");
        let hash = "z".repeat(self.config.max_hash_length.max(FALLBACK_HASH_LENGTH));
        let sample = render_id_template(
            &self.config.generation.template,
            &self.config.prefix,
            sequence,
            slug,
            &hash,
        )?;
        parse_id(&sample)?;
        Ok(())
    }

    /// Generate an ID, checking for collisions with the provided checker.
    ///
    /// The checker function should return `true` if the ID already exists.
    /// Lookup failures are returned immediately. If every candidate in the
    /// bounded collision ladder is occupied, this fails closed with
    /// [`BeadsError::IdCollision`] instead of fabricating an unchecked ID.
    ///
    /// # Errors
    ///
    /// Returns the collision checker's error, or `IdCollision` if no free ID
    /// can be found within the bounded search.
    pub fn generate<F>(
        &self,
        title: &str,
        description: Option<&str>,
        creator: Option<&str>,
        created_at: DateTime<Utc>,
        issue_count: usize,
        mut exists: F,
    ) -> Result<String>
    where
        F: FnMut(&str) -> Result<bool>,
    {
        let mut length = self.optimal_length(issue_count);

        loop {
            // Try nonces 0..10 at this length
            for nonce in 0..10 {
                let id =
                    self.generate_candidate(title, description, creator, created_at, nonce, length);
                if !exists(&id)? {
                    return Ok(id);
                }
            }

            // All nonces collided, increase length
            if length < self.config.max_hash_length {
                length += 1;
            } else {
                // The normal adaptive range is exhausted. Widen to a
                // 12-character hash and try a bounded nonce range. Every
                // returned candidate must have passed the caller's lookup.
                for nonce in 0..MAX_FALLBACK_NONCE {
                    let seed = generate_id_seed(title, description, creator, created_at, nonce);
                    let hash_str = compute_id_hash(&seed, FALLBACK_HASH_LENGTH);
                    let id = format!("{}-{hash_str}", self.config.prefix);

                    if !exists(&id)? {
                        return Ok(id);
                    }
                }

                let seed =
                    generate_id_seed(title, description, creator, created_at, MAX_FALLBACK_NONCE);
                let hash_str = compute_id_hash(&seed, FALLBACK_HASH_LENGTH);
                let id = format!("{}-{hash_str}", self.config.prefix);
                return if exists(&id)? {
                    Err(BeadsError::IdCollision { id })
                } else {
                    Ok(id)
                };
            }
        }
    }
}

#[allow(clippy::literal_string_with_formatting_args)]
fn validate_id_template(config: &IdGenerationConfig) -> Result<()> {
    if !config.allow_non_unique_template && !config.template.contains("{hash}") {
        return Err(BeadsError::validation(
            "id_generation.template",
            "templated IDs must include {hash} unless allow_non_unique_template is true",
        ));
    }
    let mut rest = config.template.as_str();
    while let Some(start) = rest.find('{') {
        if rest[..start].contains('}') {
            return Err(BeadsError::validation(
                "id_generation.template",
                "unmatched closing brace",
            ));
        }
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('}') else {
            return Err(BeadsError::validation(
                "id_generation.template",
                "unclosed template token",
            ));
        };
        let token = &after_start[..end];
        if !matches!(token, "prefix" | "seq" | "seq:03" | "slug" | "hash") {
            return Err(BeadsError::validation(
                "id_generation.template",
                format!("unknown template token '{{{token}}}'"),
            ));
        }
        rest = &after_start[end + 1..];
    }
    if rest.contains('}') {
        return Err(BeadsError::validation(
            "id_generation.template",
            "unmatched closing brace",
        ));
    }
    Ok(())
}

#[allow(clippy::literal_string_with_formatting_args)]
fn render_id_template(
    template: &str,
    prefix: &str,
    sequence: Option<IssueSequenceNumber>,
    slug: Option<&str>,
    hash: &str,
) -> Result<String> {
    let mut rendered = template.to_string();
    rendered = rendered.replace("{prefix}", prefix);
    rendered = rendered.replace("{hash}", hash);
    if rendered.contains("{slug}") {
        let slug = slug.ok_or_else(|| {
            BeadsError::validation(
                "slug",
                "id_generation.template uses {slug} but no slug was available",
            )
        })?;
        rendered = rendered.replace("{slug}", slug);
    }
    if rendered.contains("{seq:03}") {
        let sequence = sequence.ok_or_else(|| {
            BeadsError::validation(
                "id_generation.template",
                "template uses {seq:03} but no sequence was allocated",
            )
        })?;
        rendered = rendered.replace("{seq:03}", &format!("{:03}", sequence.get()));
    }
    if rendered.contains("{seq}") {
        let sequence = sequence.ok_or_else(|| {
            BeadsError::validation(
                "id_generation.template",
                "template uses {seq} but no sequence was allocated",
            )
        })?;
        rendered = rendered.replace("{seq}", &sequence.get().to_string());
    }
    if rendered.is_empty() || rendered.len() > MAX_ID_LENGTH {
        return Err(BeadsError::InvalidId { id: rendered });
    }
    if !rendered.chars().all(|c| {
        c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || c == '_'
            || c == '-'
            || c == '.'
            || c == ':'
            || c == '#'
    }) {
        return Err(BeadsError::InvalidId { id: rendered });
    }
    Ok(rendered)
}

/// Generate the seed string for ID generation.
///
/// Inputs are length-prefixed as `len:value` fields so embedded separators in
/// titles or descriptions cannot collide with adjacent fields.
#[must_use]
pub fn generate_id_seed(
    title: &str,
    description: Option<&str>,
    creator: Option<&str>,
    created_at: DateTime<Utc>,
    nonce: u32,
) -> String {
    let timestamp = created_at.timestamp_nanos_opt().unwrap_or(0).to_string();
    let nonce = nonce.to_string();

    let mut seed = String::new();
    append_seed_part(&mut seed, title);
    append_seed_part(&mut seed, description.unwrap_or(""));
    append_seed_part(&mut seed, creator.unwrap_or(""));
    append_seed_part(&mut seed, &timestamp);
    append_seed_part(&mut seed, &nonce);
    seed
}

fn append_seed_part(seed: &mut String, value: &str) {
    use std::fmt::Write;
    write!(seed, "{}:", value.len()).expect("writing to String never fails");
    seed.push_str(value);
}

/// Compute a base36 hash of the input string with a specific length.
///
/// Uses SHA256 to hash the input, then converts the first 8 bytes to a u64,
/// encodes as base36, and truncates to the requested length.
#[must_use]
pub fn compute_id_hash(input: &str, length: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();

    // Use first 8 bytes for a 64-bit integer
    let mut num = 0u64;
    for &byte in result.iter().take(8) {
        num = (num << 8) | u64::from(byte);
    }

    let encoded = base36_encode(num);

    // Pad with '0' if too short (unlikely for u64 but possible)
    let mut s = encoded;
    if s.len() < length {
        s = format!("{s:0>length$}");
    }

    // Take the last `length` characters to ensure full entropy from
    // the least significant digits of the base36 encoding.
    let start = s.len().saturating_sub(length);
    s.chars().skip(start).collect()
}

fn base36_encode(mut num: u64) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if num == 0 {
        return "0".to_string();
    }
    let mut chars = Vec::new();
    while num > 0 {
        chars.push(ALPHABET[(num % 36) as usize] as char);
        num /= 36;
    }
    chars.into_iter().rev().collect()
}

// ============================================================================
// Child ID Helpers
// ============================================================================

/// Generate child ID from parent.
///
/// Child IDs have format: `<parent>.<n>` where n is the child number.
#[must_use]
pub fn child_id(parent_id: &str, child_number: u32) -> String {
    format!("{parent_id}.{child_number}")
}

fn issue_id_separator(id: &str) -> Option<usize> {
    id.rfind('-')
}

pub(crate) fn split_prefix_remainder(id: &str) -> Option<(&str, &str)> {
    let dash_pos = issue_id_separator(id)?;
    let (prefix, remainder_with_dash) = id.split_at(dash_pos);
    let remainder = remainder_with_dash.strip_prefix('-')?;
    if prefix.is_empty() || remainder.is_empty() {
        return None;
    }
    Some((prefix, remainder))
}

/// Check if an ID is a child ID (contains a dot after the hash).
#[must_use]
pub fn is_child_id(id: &str) -> bool {
    // Only check after the prefix-hash part
    split_prefix_remainder(id).map_or_else(
        || id.contains('.'),
        |(_, remainder)| remainder.contains('.'),
    )
}

/// Get the depth of a hierarchical ID.
///
/// Top-level IDs have depth 0, first-level children have depth 1, etc.
#[must_use]
pub fn id_depth(id: &str) -> usize {
    // Count dots after the prefix-hash part
    split_prefix_remainder(id).map_or_else(
        || id.matches('.').count(),
        |(_, remainder)| remainder.matches('.').count(),
    )
}

// ============================================================================
// ID Parsing and Validation
// ============================================================================

use crate::error::{BeadsError, Result};

/// Parsed components of an issue ID.
///
/// Supports both root IDs (`br-abc123`) and hierarchical IDs (`br-abc123.1.2`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedId {
    /// The prefix (e.g., "br").
    pub prefix: String,
    /// The hash portion (e.g., "abc123").
    pub hash: String,
    /// Child path segments if this is a hierarchical ID (e.g., `[1, 2]` for `.1.2`).
    pub child_path: Vec<u32>,
}

impl ParsedId {
    /// Returns true if this is a root (non-hierarchical) ID.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.child_path.is_empty()
    }

    /// Returns the depth in the hierarchy (0 for root).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.child_path.len()
    }

    /// Get the parent ID if this is a child.
    ///
    /// Returns `None` for root IDs.
    #[must_use]
    pub fn parent(&self) -> Option<String> {
        if self.child_path.is_empty() {
            return None;
        }

        let mut parent_path = self.child_path.clone();
        parent_path.pop();

        if parent_path.is_empty() {
            Some(format!("{}-{}", self.prefix, self.hash))
        } else {
            let path_str = format_child_path(&parent_path);
            Some(format!("{}-{}{}", self.prefix, self.hash, path_str))
        }
    }

    /// Reconstruct the full ID string.
    #[must_use]
    pub fn to_id_string(&self) -> String {
        if self.child_path.is_empty() {
            format!("{}-{}", self.prefix, self.hash)
        } else {
            let path_str = format_child_path(&self.child_path);
            format!("{}-{}{}", self.prefix, self.hash, path_str)
        }
    }

    /// Check if this ID is a child (direct or indirect) of another.
    #[must_use]
    pub fn is_child_of(&self, potential_parent: &str) -> bool {
        let full_id = self.to_id_string();
        full_id.starts_with(potential_parent)
            && full_id.len() > potential_parent.len()
            && full_id[potential_parent.len()..].starts_with('.')
    }
}

fn format_child_path(path: &[u32]) -> String {
    let mut out = String::new();
    for segment in path {
        use std::fmt::Write;
        let _ = write!(out, ".{segment}");
    }
    out
}

/// Parse an issue ID into its components.
///
/// # Errors
///
/// Returns `InvalidId` if the ID format is invalid.
pub fn parse_id(id: &str) -> Result<ParsedId> {
    // Find the prefix-hash separator (supports hyphenated prefixes)
    let Some((prefix, remainder)) = split_prefix_remainder(id) else {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    };

    if prefix.is_empty() || prefix.len() > MAX_ID_PREFIX_LEN {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    }

    if !prefix.chars().all(|c| {
        c.is_ascii_lowercase()
            || c.is_ascii_digit()
            || c == '_'
            || c == '-'
            || c == '.'
            || c == ':'
            || c == '#'
    }) {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    }

    // Split remainder by '.' to get hash and child path
    let parts: Vec<&str> = remainder.split('.').collect();
    let hash = parts[0].to_string();

    if hash.is_empty() || hash.len() > MAX_ID_HASH_LEN {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    }

    // Validate hash is base36 (lowercase alphanumeric)
    if !hash
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(BeadsError::InvalidId { id: id.to_string() });
    }

    // Parse child path segments
    let mut child_path = Vec::new();
    for part in parts.iter().skip(1) {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return Err(BeadsError::InvalidId { id: id.to_string() });
        }
        match part.parse::<u32>() {
            Ok(n) => child_path.push(n),
            Err(_) => return Err(BeadsError::InvalidId { id: id.to_string() }),
        }
    }

    Ok(ParsedId {
        prefix: prefix.to_string(),
        hash,
        child_path,
    })
}

/// Validate that an ID has the expected prefix.
///
/// # Arguments
///
/// * `id` - The ID to validate
/// * `expected_prefix` - The primary expected prefix
/// * `allowed_prefixes` - Additional allowed prefixes
///
/// # Errors
///
/// Returns `PrefixMismatch` if the prefix doesn't match expected or allowed.
pub fn validate_prefix(id: &str, expected_prefix: &str, allowed_prefixes: &[String]) -> Result<()> {
    let parsed = parse_id(id)?;

    if parsed.prefix == expected_prefix {
        return Ok(());
    }

    if allowed_prefixes.contains(&parsed.prefix) {
        return Ok(());
    }

    Err(BeadsError::PrefixMismatch {
        expected: expected_prefix.to_string(),
        found: parsed.prefix,
    })
}

/// Normalize an ID to consistent lowercase format.
#[must_use]
pub fn normalize_id(id: &str) -> String {
    id.to_lowercase()
}

fn normalized_prefix_chars(prefix: &str) -> String {
    let normalized: String = prefix
        .trim()
        .chars()
        .filter_map(|c| {
            let normalized = c.to_ascii_lowercase();
            (normalized.is_ascii_lowercase()
                || normalized.is_ascii_digit()
                || matches!(normalized, '_' | '-' | '.' | ':' | '#'))
            .then_some(normalized)
        })
        .take(MAX_ID_PREFIX_LEN)
        .collect();

    normalized
        .trim_end_matches(['_', '-', '.', ':', '#'])
        .to_string()
}

/// Normalize an inferred issue prefix into a valid runtime form.
///
/// This trims whitespace, lowercases ASCII letters, removes unsupported
/// characters, and clamps the prefix to the maximum supported length. Inferred
/// prefixes that contain no usable characters fall back to `br`; explicitly
/// configured prefixes should use [`normalize_configured_prefix`] instead.
#[must_use]
pub fn normalize_prefix(prefix: &str) -> String {
    let normalized = normalized_prefix_chars(prefix);

    if normalized.is_empty() {
        "br".to_string()
    } else {
        normalized
    }
}

/// Normalize an explicitly configured issue prefix.
///
/// Unlike [`normalize_prefix`], this rejects values that contain no usable
/// prefix characters so a typo cannot silently mint IDs in the `br` namespace.
///
/// # Errors
///
/// Returns a configuration error when the configured value is empty after
/// normalization.
pub fn normalize_configured_prefix(prefix: &str) -> Result<String> {
    let normalized = normalized_prefix_chars(prefix);
    if normalized.is_empty() {
        return Err(BeadsError::Config(format!(
            "issue_prefix {prefix:?} contains no usable ASCII letters, digits, or separators"
        )));
    }
    Ok(normalized)
}

/// Maximum length of a normalized slug (the user-supplied human-readable
/// segment of an ID). Capped well below `MAX_ID_PREFIX_LEN` so the
/// `<prefix>-<slug>` portion still fits within the parser's prefix budget.
pub const MAX_SLUG_LEN: usize = 48;

/// Normalize a user-supplied slug for embedding in an issue ID.
///
/// The output is lowercase ASCII alphanumerics and hyphens only. Runs of
/// any other characters collapse to a single hyphen. Leading and trailing
/// hyphens are stripped. The result is capped at [`MAX_SLUG_LEN`] characters
/// and then re-trimmed of any trailing hyphen the cap may have left behind.
///
/// Returns an empty string if no usable characters remain — callers must
/// fall back to the hash-only ID path in that case.
#[must_use]
pub fn normalize_slug(slug: &str) -> String {
    let mut out = String::with_capacity(slug.len());
    let mut prev_was_hyphen = false;
    for c in slug.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_lowercase() || lc.is_ascii_digit() {
            out.push(lc);
            prev_was_hyphen = false;
        } else if !prev_was_hyphen && !out.is_empty() {
            out.push('-');
            prev_was_hyphen = true;
        }
    }

    // Trim trailing hyphen that any final non-alphanumeric run may have
    // appended, and cap length.
    while out.ends_with('-') {
        out.pop();
    }
    if out.len() > MAX_SLUG_LEN {
        out.truncate(MAX_SLUG_LEN);
        while out.ends_with('-') {
            out.pop();
        }
    }
    out
}

fn normalize_slug_for_prefix(slug: &str, prefix: &str) -> String {
    let mut normalized = normalize_slug(slug);
    let Some(max_len) = MAX_ID_PREFIX_LEN
        .checked_sub(prefix.len())
        .and_then(|remaining| remaining.checked_sub(1))
    else {
        return String::new();
    };

    if normalized.len() > max_len {
        normalized.truncate(max_len);
        while normalized.ends_with('-') {
            normalized.pop();
        }
    }
    normalized
}

/// Abbreviate a long auto-derived issue ID prefix.
///
/// If the normalized prefix is short enough (<= 6 chars), it is returned
/// unchanged. Otherwise, multi-word prefixes are abbreviated to their initials
/// and single-word prefixes fall back to their first three alphanumeric chars.
#[must_use]
pub fn abbreviate_prefix(prefix: &str) -> String {
    let normalized = normalize_prefix(prefix);
    if normalized.len() <= 6 {
        return normalized;
    }

    let segments: Vec<&str> = normalized
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .collect();

    if segments.len() > 1 {
        let abbrev: String = segments
            .iter()
            .filter_map(|segment| segment.chars().next())
            .collect();
        if abbrev.len() > 1 {
            return abbrev;
        }
    }

    let fallback: String = normalized
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(3)
        .collect();
    if fallback.is_empty() {
        normalized
    } else {
        fallback
    }
}

/// Check if a string looks like a valid issue ID format.
#[must_use]
pub fn is_valid_id_format(id: &str) -> bool {
    parse_id(id).is_ok()
}

// ============================================================================
// ID Resolution
// ============================================================================

/// Configuration for ID resolution.
#[derive(Debug, Clone)]
pub struct ResolverConfig {
    /// Default prefix to use when input lacks one.
    pub default_prefix: String,
    /// Additional allowed prefixes for matching.
    pub allowed_prefixes: Vec<String>,
    /// Whether to allow substring matching on hash portion.
    pub allow_substring_match: bool,
}

impl Default for ResolverConfig {
    fn default() -> Self {
        Self {
            default_prefix: "br".to_string(),
            allowed_prefixes: Vec::new(),
            allow_substring_match: true,
        }
    }
}

impl ResolverConfig {
    /// Create a new resolver config with the given default prefix.
    #[must_use]
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self {
            default_prefix: normalize_prefix(&prefix.into()),
            ..Default::default()
        }
    }
}

/// Resolved ID result from the resolution process.
#[derive(Debug, Clone)]
pub struct ResolvedId {
    /// The full resolved ID.
    pub id: String,
    /// How the ID was matched.
    pub match_type: MatchType,
    /// The original input that was resolved.
    pub original_input: String,
}

/// How an ID was matched during resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchType {
    /// Exact match on full ID.
    Exact,
    /// Matched after prepending the default prefix.
    PrefixNormalized,
    /// Matched via substring on hash portion.
    Substring,
}

/// ID resolver that resolves partial IDs to full IDs.
///
/// Resolution order:
/// 1. Exact ID match
/// 2. Normalize: if missing prefix, prepend `default_prefix-` and retry
/// 3. Substring match on hash portion across all prefixes
/// 4. Ambiguity => error with candidate list
#[derive(Debug, Clone)]
pub struct IdResolver {
    config: ResolverConfig,
}

impl IdResolver {
    /// Create a new ID resolver with the given config.
    #[must_use]
    pub const fn new(config: ResolverConfig) -> Self {
        Self { config }
    }

    /// Create a new ID resolver with default config.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(ResolverConfig::default())
    }

    /// Create a new ID resolver with the given default prefix.
    #[must_use]
    pub fn with_prefix(prefix: impl Into<String>) -> Self {
        Self::new(ResolverConfig::with_prefix(prefix))
    }

    /// Get the default prefix.
    #[must_use]
    pub fn default_prefix(&self) -> &str {
        &self.config.default_prefix
    }

    /// Resolve a partial ID to a full ID.
    ///
    /// The `lookup_fn` should return all IDs that match the given pattern.
    /// - For exact match, pass the ID and expect 0 or 1 results.
    /// - For substring match, pass the pattern and expect 0-N results.
    ///
    /// The `exists_fn` should check if an exact ID exists.
    ///
    /// # Errors
    ///
    /// - `IssueNotFound` if no match is found.
    /// - `AmbiguousId` if multiple matches are found.
    ///
    /// # Panics
    ///
    /// This function will not panic under normal operation. The internal
    /// `.expect()` call is guarded by a length check ensuring exactly one match exists.
    pub fn resolve<F, G>(
        &self,
        input: &str,
        exists_fn: F,
        substring_match_fn: G,
    ) -> Result<ResolvedId>
    where
        F: Fn(&str) -> bool,
        G: Fn(&str) -> Vec<String>,
    {
        let input = input.trim();

        if input.is_empty() {
            return Err(BeadsError::InvalidId { id: String::new() });
        }

        // Normalize input to lowercase
        let normalized = normalize_id(input);

        // Step 1: Try exact match
        if exists_fn(&normalized) {
            return Ok(ResolvedId {
                id: normalized,
                match_type: MatchType::Exact,
                original_input: input.to_string(),
            });
        }

        // Step 2: If no dash (missing prefix), prepend default prefix and retry
        if !normalized.contains('-') {
            let with_prefix = format!("{}-{}", self.config.default_prefix, normalized);
            if exists_fn(&with_prefix) {
                return Ok(ResolvedId {
                    id: with_prefix,
                    match_type: MatchType::PrefixNormalized,
                    original_input: input.to_string(),
                });
            }
        }

        // Step 3: Substring match on hash portion
        if self.config.allow_substring_match {
            // Extract the potential hash portion (after dash, or entire input if no dash)
            let (prefix, hash_pattern) = split_prefix_remainder(&normalized)
                .map_or((None, normalized.as_str()), |(p, r)| (Some(p), r));

            if !hash_pattern.is_empty() {
                let mut matches = substring_match_fn(hash_pattern);

                if let Some(p) = prefix {
                    let expected_prefix = format!("{p}-");
                    matches.retain(|id| id.starts_with(&expected_prefix));
                }

                match matches.len() {
                    0 => {
                        // No matches found
                    }
                    1 => {
                        return Ok(ResolvedId {
                            id: matches.into_iter().next().unwrap_or_default(),
                            match_type: MatchType::Substring,
                            original_input: input.to_string(),
                        });
                    }
                    _ => {
                        // Multiple matches - ambiguous
                        return Err(BeadsError::AmbiguousId {
                            partial: input.to_string(),
                            matches,
                        });
                    }
                }
            }
        }

        // Step 4: No match found
        Err(BeadsError::IssueNotFound {
            id: input.to_string(),
        })
    }

    /// Resolve a partial ID while allowing the lookup callbacks to fail.
    ///
    /// This is the same algorithm as [`IdResolver::resolve`], but it preserves
    /// storage/query errors instead of forcing callers to coerce them into
    /// `false` or an empty match set.
    ///
    /// # Errors
    ///
    /// - Propagates any error returned by `exists_fn` or `substring_match_fn`
    /// - `IssueNotFound` if no match is found
    /// - `AmbiguousId` if multiple matches are found
    pub fn resolve_fallible<F, G>(
        &self,
        input: &str,
        exists_fn: F,
        substring_match_fn: G,
    ) -> Result<ResolvedId>
    where
        F: Fn(&str) -> Result<bool>,
        G: Fn(&str) -> Result<Vec<String>>,
    {
        let input = input.trim();

        if input.is_empty() {
            return Err(BeadsError::InvalidId { id: String::new() });
        }

        let normalized = normalize_id(input);

        if exists_fn(&normalized)? {
            return Ok(ResolvedId {
                id: normalized,
                match_type: MatchType::Exact,
                original_input: input.to_string(),
            });
        }

        if !normalized.contains('-') {
            let with_prefix = format!("{}-{}", self.config.default_prefix, normalized);
            if exists_fn(&with_prefix)? {
                return Ok(ResolvedId {
                    id: with_prefix,
                    match_type: MatchType::PrefixNormalized,
                    original_input: input.to_string(),
                });
            }
        }

        if self.config.allow_substring_match {
            let (prefix, hash_pattern) = split_prefix_remainder(&normalized)
                .map_or((None, normalized.as_str()), |(p, r)| (Some(p), r));

            if !hash_pattern.is_empty() {
                let mut matches = substring_match_fn(hash_pattern)?;

                if let Some(p) = prefix {
                    let expected_prefix = format!("{p}-");
                    matches.retain(|id| id.starts_with(&expected_prefix));
                }

                match matches.len() {
                    0 => {}
                    1 => {
                        return Ok(ResolvedId {
                            id: matches.into_iter().next().unwrap_or_default(),
                            match_type: MatchType::Substring,
                            original_input: input.to_string(),
                        });
                    }
                    _ => {
                        return Err(BeadsError::AmbiguousId {
                            partial: input.to_string(),
                            matches,
                        });
                    }
                }
            }
        }

        Err(BeadsError::IssueNotFound {
            id: input.to_string(),
        })
    }

    /// Resolve multiple IDs, returning results for each.
    ///
    /// If any ID fails to resolve, returns the first error.
    ///
    /// # Errors
    ///
    /// Returns the first error encountered if any ID fails to resolve.
    /// See [`IdResolver::resolve`] for the specific error conditions.
    pub fn resolve_all<F, G>(
        &self,
        inputs: &[String],
        exists_fn: F,
        substring_match_fn: G,
    ) -> Result<Vec<ResolvedId>>
    where
        F: Fn(&str) -> bool,
        G: Fn(&str) -> Vec<String>,
    {
        inputs
            .iter()
            .map(|input| self.resolve(input, &exists_fn, &substring_match_fn))
            .collect()
    }

    /// Resolve multiple IDs while preserving lookup callback errors.
    ///
    /// # Errors
    ///
    /// Returns the first callback or resolution error encountered.
    pub fn resolve_all_fallible<F, G>(
        &self,
        inputs: &[String],
        exists_fn: F,
        substring_match_fn: G,
    ) -> Result<Vec<ResolvedId>>
    where
        F: Fn(&str) -> Result<bool>,
        G: Fn(&str) -> Result<Vec<String>>,
    {
        inputs
            .iter()
            .map(|input| self.resolve_fallible(input, &exists_fn, &substring_match_fn))
            .collect()
    }
}

/// Find all issue IDs that contain the given substring in their hash portion.
///
/// This is a helper function for implementing the `substring_match_fn` parameter
/// of `IdResolver::resolve`. The caller provides the list of all known IDs.
#[must_use]
pub fn find_matching_ids(all_ids: &[String], hash_substring: &str) -> Vec<String> {
    // Split search pattern into base hash and optional child path so that
    // searching for "64up6.4" correctly matches "bd-64up6.4" instead of
    // stripping the child suffix from the candidate and failing.
    let (search_base, search_child) = match hash_substring.split_once('.') {
        Some((base, child)) => (base, Some(child)),
        None => (hash_substring, None),
    };

    all_ids
        .iter()
        .filter(|id| {
            split_prefix_remainder(id).is_some_and(|(_, remainder)| {
                let base_hash = remainder.split('.').next().unwrap_or(remainder);
                if !base_hash.contains(search_base) {
                    return false;
                }
                match search_child {
                    Some(child) => remainder
                        .split_once('.')
                        .is_some_and(|(_, candidate_child)| candidate_child == child),
                    None => true,
                }
            })
        })
        .cloned()
        .collect()
}

/// Quick helper to resolve a single ID with default settings.
///
/// This is useful for simple cases where you just need to resolve one ID.
///
/// # Errors
///
/// - `IssueNotFound` if no match is found.
/// - `AmbiguousId` if multiple matches are found.
/// - `InvalidId` if the input is empty.
pub fn resolve_id<F, G>(input: &str, exists_fn: F, substring_match_fn: G) -> Result<String>
where
    F: Fn(&str) -> bool,
    G: Fn(&str) -> Vec<String>,
{
    let resolver = IdResolver::with_defaults();
    resolver
        .resolve(input, exists_fn, substring_match_fn)
        .map(|r| r.id)
}

/// Quick helper to resolve a single ID with fallible lookup callbacks.
///
/// # Errors
///
/// Propagates any lookup error in addition to the usual resolution errors.
pub fn resolve_id_fallible<F, G>(input: &str, exists_fn: F, substring_match_fn: G) -> Result<String>
where
    F: Fn(&str) -> Result<bool>,
    G: Fn(&str) -> Result<Vec<String>>,
{
    let resolver = IdResolver::with_defaults();
    resolver
        .resolve_fallible(input, exists_fn, substring_match_fn)
        .map(|r| r.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // ID Resolution Tests
    // ========================================================================

    fn mock_db() -> Vec<String> {
        vec![
            "br-abc123".to_string(),
            "br-abd456".to_string(),
            "br-xyz789".to_string(),
            "br-abc123.1".to_string(),  // child
            "other-def111".to_string(), // different prefix
        ]
    }

    fn exists_in_mock(id: &str) -> bool {
        mock_db().contains(&id.to_string())
    }

    fn substring_in_mock(pattern: &str) -> Vec<String> {
        find_matching_ids(&mock_db(), pattern)
    }

    #[test]
    fn test_resolve_exact_match() {
        let resolver = IdResolver::with_defaults();
        let result = resolver
            .resolve("br-abc123", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123");
        assert_eq!(result.match_type, MatchType::Exact);
    }

    #[test]
    fn test_resolve_prefix_normalized() {
        let resolver = IdResolver::with_defaults();
        let result = resolver
            .resolve("abc123", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123");
        assert_eq!(result.match_type, MatchType::PrefixNormalized);
    }

    #[test]
    fn test_resolve_substring_match() {
        let resolver = IdResolver::with_defaults();
        // "xyz" should uniquely match "br-xyz789"
        let result = resolver
            .resolve("xyz", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-xyz789");
        assert_eq!(result.match_type, MatchType::Substring);
    }

    #[test]
    fn test_resolve_ambiguous() {
        let resolver = IdResolver::with_defaults();
        // "ab" matches both "br-abc123" and "br-abd456"
        let result = resolver.resolve("ab", exists_in_mock, substring_in_mock);
        assert!(result.is_err());
        if let Err(BeadsError::AmbiguousId { partial, matches }) = result {
            assert_eq!(partial, "ab");
            assert!(matches.contains(&"br-abc123".to_string()));
            assert!(matches.contains(&"br-abd456".to_string()));
        } else {
            unreachable!("Expected AmbiguousId error");
        }
    }

    #[test]
    fn test_resolve_not_found() {
        let resolver = IdResolver::with_defaults();
        let result = resolver.resolve("nonexistent", exists_in_mock, substring_in_mock);
        assert!(result.is_err());
        if let Err(BeadsError::IssueNotFound { id }) = result {
            assert_eq!(id, "nonexistent");
        } else {
            unreachable!("Expected IssueNotFound error");
        }
    }

    #[test]
    fn test_resolve_child_id() {
        let resolver = IdResolver::with_defaults();
        let result = resolver
            .resolve("br-abc123.1", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123.1");
        assert_eq!(result.match_type, MatchType::Exact);
    }

    #[test]
    fn test_resolve_case_insensitive() {
        let resolver = IdResolver::with_defaults();
        let result = resolver
            .resolve("BR-ABC123", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123");
    }

    #[test]
    fn test_resolve_with_custom_prefix() {
        let custom_db = vec!["proj-aaa111".to_string()];
        let exists = |id: &str| custom_db.contains(&id.to_string());
        let substring = |pattern: &str| find_matching_ids(&custom_db, pattern);

        let resolver = IdResolver::with_prefix("proj");
        let result = resolver.resolve("aaa111", exists, substring).unwrap();
        assert_eq!(result.id, "proj-aaa111");
        assert_eq!(result.match_type, MatchType::PrefixNormalized);
    }

    #[test]
    fn test_resolve_empty_input() {
        let resolver = IdResolver::with_defaults();
        let result = resolver.resolve("", exists_in_mock, substring_in_mock);
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_whitespace_trimmed() {
        let resolver = IdResolver::with_defaults();
        let result = resolver
            .resolve("  br-abc123  ", exists_in_mock, substring_in_mock)
            .unwrap();
        assert_eq!(result.id, "br-abc123");
    }

    #[test]
    fn test_resolve_fallible_propagates_lookup_error() {
        let resolver = IdResolver::with_defaults();
        let result = resolver.resolve_fallible(
            "br-abc123",
            |_id| Err(BeadsError::Config("lookup failed".to_string())),
            |_hash| Ok(Vec::new()),
        );
        assert!(matches!(result, Err(BeadsError::Config(message)) if message == "lookup failed"));
    }

    #[test]
    fn test_resolve_all_fallible_propagates_lookup_error() {
        let resolver = IdResolver::with_defaults();
        let inputs = vec!["br-abc123".to_string(), "br-xyz789".to_string()];
        let result = resolver.resolve_all_fallible(
            &inputs,
            |_id| Err(BeadsError::Config("exists lookup failed".to_string())),
            |_hash| Ok(Vec::new()),
        );
        assert!(
            matches!(result, Err(BeadsError::Config(message)) if message == "exists lookup failed")
        );
    }

    #[test]
    fn test_find_matching_ids_substring() {
        let ids = mock_db();
        let matches = find_matching_ids(&ids, "abc");
        assert!(matches.contains(&"br-abc123".to_string()));
        // Note: br-abc123.1 is a child and its base hash contains "abc"
        assert!(matches.contains(&"br-abc123.1".to_string()));
    }

    #[test]
    fn test_find_matching_ids_no_match() {
        let ids = mock_db();
        let matches = find_matching_ids(&ids, "zzz");
        assert!(matches.is_empty());
    }

    // ========================================================================
    // Original Tests
    // ========================================================================

    #[test]
    fn test_base36_encode() {
        assert_eq!(base36_encode(0), "0");
        assert_eq!(base36_encode(10), "a");
        assert_eq!(base36_encode(35), "z");
        assert_eq!(base36_encode(36), "10");
    }

    #[test]
    fn test_compute_id_hash_length() {
        let input = "test input";
        let hash3 = compute_id_hash(input, 3);
        assert_eq!(hash3.len(), 3);

        let hash8 = compute_id_hash(input, 8);
        assert_eq!(hash8.len(), 8);
    }

    #[test]
    fn test_generate_id_seed() {
        let now = Utc::now();
        let seed = generate_id_seed("title", Some("desc"), Some("me"), now, 0);
        assert!(seed.contains("5:title"));
        assert!(seed.contains("4:desc"));
        assert!(seed.contains("2:me"));
        assert!(seed.ends_with("1:0"));
    }

    #[test]
    fn test_parse_id_root() {
        let parsed = parse_id("bd-abc123").unwrap();
        assert_eq!(parsed.prefix, "bd");
        assert_eq!(parsed.hash, "abc123");
        assert!(parsed.child_path.is_empty());
        assert!(parsed.is_root());
        assert_eq!(parsed.depth(), 0);
    }

    #[test]
    fn test_parse_id_hyphenated_prefix() {
        let parsed = parse_id("bead-me-up-3e9").unwrap();
        assert_eq!(parsed.prefix, "bead-me-up");
        assert_eq!(parsed.hash, "3e9");
        assert!(parsed.child_path.is_empty());

        let parsed2 = parse_id("document-intelligence-0sa.2").unwrap();
        assert_eq!(parsed2.prefix, "document-intelligence");
        assert_eq!(parsed2.hash, "0sa");
        assert_eq!(parsed2.child_path, vec![2]);
    }

    #[test]
    fn test_parse_id_hyphenated_prefix_word_like_hash() {
        // "my-proj" is prefix. "abcd" is hash (4 chars, no digits).
        // Previously failed because "abcd" was deemed unlikely hash, causing split at first dash.
        let parsed = parse_id("my-proj-abcd").unwrap();
        assert_eq!(parsed.prefix, "my-proj");
        assert_eq!(parsed.hash, "abcd");
    }

    #[test]
    fn test_parse_id_child() {
        let parsed = parse_id("bd-abc123.1").unwrap();
        assert_eq!(parsed.prefix, "bd");
        assert_eq!(parsed.hash, "abc123");
        assert_eq!(parsed.child_path, vec![1]);
        assert!(!parsed.is_root());
        assert_eq!(parsed.depth(), 1);
    }

    #[test]
    fn test_parse_id_grandchild() {
        let parsed = parse_id("bd-abc123.1.2").unwrap();
        assert_eq!(parsed.child_path, vec![1, 2]);
        assert_eq!(parsed.depth(), 2);
    }

    #[test]
    fn test_parse_id_external_style() {
        let parsed = parse_id("external:jira-123").unwrap();
        assert_eq!(parsed.prefix, "external:jira");
        assert_eq!(parsed.hash, "123");

        let parsed2 = parse_id("ext:github#repo-456").unwrap();
        assert_eq!(parsed2.prefix, "ext:github#repo");
        assert_eq!(parsed2.hash, "456");
    }

    #[test]
    fn test_parse_id_invalid_no_dash() {
        assert!(parse_id("bdabc123").is_err());
    }

    #[test]
    fn test_parse_id_invalid_empty_hash() {
        assert!(parse_id("bd-").is_err());
    }

    #[test]
    fn test_parse_id_invalid_uppercase() {
        assert!(parse_id("bd-ABC123").is_err());
    }

    #[test]
    fn test_parse_id_long_hash() {
        // Fallback generates 12 chars + potential nonce.
        // parse_id used to limit to 8, but now allows up to MAX_ID_HASH_LEN.
        let long_id = "bd-abc123456789";
        let parsed = parse_id(long_id).unwrap();
        assert_eq!(parsed.hash, "abc123456789");
    }

    #[test]
    fn test_parsed_id_parent() {
        let child = parse_id("bd-abc123.1").unwrap();
        assert_eq!(child.parent(), Some("bd-abc123".to_string()));

        let grandchild = parse_id("bd-abc123.1.2").unwrap();
        assert_eq!(grandchild.parent(), Some("bd-abc123.1".to_string()));

        let root = parse_id("bd-abc123").unwrap();
        assert_eq!(root.parent(), None);
    }

    #[test]
    fn test_parsed_id_to_string() {
        let root = parse_id("bd-abc123").unwrap();
        assert_eq!(root.to_id_string(), "bd-abc123");

        let child = parse_id("bd-abc123.1.2").unwrap();
        assert_eq!(child.to_id_string(), "bd-abc123.1.2");
    }

    #[test]
    fn test_parsed_id_is_child_of() {
        let child = parse_id("bd-abc123.1").unwrap();
        assert!(child.is_child_of("bd-abc123"));
        assert!(!child.is_child_of("bd-xyz"));

        let grandchild = parse_id("bd-abc123.1.2").unwrap();
        assert!(grandchild.is_child_of("bd-abc123"));
        assert!(grandchild.is_child_of("bd-abc123.1"));
    }

    #[test]
    fn test_validate_prefix() {
        assert!(validate_prefix("bd-abc123", "bd", &[]).is_ok());
        assert!(validate_prefix("bd-abc123", "other", &["bd".to_string()]).is_ok());
        assert!(validate_prefix("bd-abc123", "other", &[]).is_err());
    }

    #[test]
    fn test_normalize_prefix_sanitizes_and_lowercases() {
        assert_eq!(normalize_prefix("  Project-Name_2!  "), "project-name_2");
        assert_eq!(normalize_prefix("!!!"), "br");
    }

    #[test]
    fn test_configured_prefix_rejects_values_without_usable_characters() {
        for prefix in ["", "   ", "!!!", "日本語", "___", "...", "###"] {
            let err = normalize_configured_prefix(prefix)
                .expect_err("configured prefix must not silently fall back");
            assert!(matches!(err, BeadsError::Config(_)));
        }

        assert_eq!(
            normalize_configured_prefix(" Project-Name_2! ").unwrap(),
            "project-name_2"
        );
        assert!(IdConfig::with_prefix("!!!").is_err());
    }

    #[test]
    fn test_abbreviate_prefix_handles_mixed_case_and_underscores() {
        assert_eq!(abbreviate_prefix("My_Project-Name"), "mpn");
        assert_eq!(abbreviate_prefix("superlongname"), "sup");
    }

    #[test]
    fn test_is_valid_id_format() {
        assert!(is_valid_id_format("bd-abc123"));
        assert!(is_valid_id_format("bd-abc123.1.2"));
        assert!(!is_valid_id_format("invalid"));
        assert!(!is_valid_id_format("bd-ABC")); // uppercase
    }

    #[test]
    fn test_id_generator_optimal_length() {
        let id_gen = IdGenerator::with_defaults();

        // Small DB should use minimum length
        assert_eq!(id_gen.optimal_length(0), 3);
        assert_eq!(id_gen.optimal_length(10), 3);

        // Large DB should need more characters
        let len_1000 = id_gen.optimal_length(1000);
        assert!(len_1000 >= 3);
        assert!(len_1000 <= 8);
    }

    #[test]
    fn test_id_generator_generate() {
        let id_gen = IdGenerator::with_defaults();
        let now = Utc::now();

        let id = id_gen
            .generate(
                "Test Issue",
                Some("Description"),
                Some("user"),
                now,
                0,
                |_| Ok(false),
            )
            .expect("collision lookup succeeds");

        assert!(id.starts_with("br-"));
        assert!(is_valid_id_format(&id));
    }

    #[test]
    fn test_id_generator_collision_handling() {
        let id_gen = IdGenerator::with_defaults();
        let now = Utc::now();

        let mut generated = std::collections::HashSet::new();

        // Generate first ID
        let id1 = id_gen
            .generate("Test", None, None, now, 0, |id| Ok(generated.contains(id)))
            .expect("collision lookup succeeds");
        generated.insert(id1.clone());

        // Generate second ID - should get different one due to collision check
        let id2 = id_gen
            .generate("Test", None, None, now, 0, |id| Ok(generated.contains(id)))
            .expect("collision lookup succeeds");

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_id_generator_propagates_collision_lookup_failure() {
        let id_gen = IdGenerator::with_defaults();
        let expected = "collision lookup unavailable";

        let err = id_gen
            .generate("Test", None, None, Utc::now(), 0, |_| {
                Err(BeadsError::Config(expected.to_string()))
            })
            .expect_err("lookup failure must stop generation immediately");

        assert!(matches!(err, BeadsError::Config(message) if message == expected));
    }

    #[test]
    fn test_id_generator_fails_closed_when_every_candidate_exists() {
        let id_gen = IdGenerator::new(IdConfig {
            prefix: "br".to_string(),
            min_hash_length: 3,
            max_hash_length: 3,
            max_collision_prob: 0.25,
            generation: IdGenerationConfig::default(),
        });
        let calls = std::cell::Cell::new(0_u32);

        let err = id_gen
            .generate("Test", None, None, Utc::now(), 0, |_| {
                calls.set(calls.get() + 1);
                Ok(true)
            })
            .expect_err("a saturated checker must fail closed");

        assert!(matches!(err, BeadsError::IdCollision { .. }));
        assert_eq!(calls.get(), 10 + MAX_FALLBACK_NONCE + 1);
    }

    #[test]
    fn test_normalize_slug_basic() {
        assert_eq!(normalize_slug("survey-my-thing"), "survey-my-thing");
        assert_eq!(normalize_slug("Survey My Thing"), "survey-my-thing");
        assert_eq!(normalize_slug("---survey---"), "survey");
        assert_eq!(normalize_slug("a/b/c"), "a-b-c");
        assert_eq!(normalize_slug("a   b   c"), "a-b-c");
        assert_eq!(normalize_slug(""), "");
        assert_eq!(normalize_slug("!!!"), "");
        assert_eq!(normalize_slug("!@#$abc"), "abc");
    }

    #[test]
    fn test_normalize_slug_collapses_unicode_and_caps_length() {
        // Non-ASCII characters are dropped (not transliterated). Long inputs
        // are capped at MAX_SLUG_LEN with no trailing hyphen.
        assert_eq!(normalize_slug("café-résumé"), "caf-r-sum");
        let long = "a".repeat(100);
        let out = normalize_slug(&long);
        assert_eq!(out.len(), MAX_SLUG_LEN);
        assert!(!out.ends_with('-'));
    }

    #[test]
    fn test_id_generator_generate_with_slug() {
        let id_gen = IdGenerator::new(IdConfig::default());
        let now = Utc::now();
        let input = IdGenerationInput {
            title: "title",
            description: None,
            creator: None,
            created_at: now,
            issue_count: 10,
        };

        // Slug present → ID embeds the slug between prefix and hash.
        let id = id_gen
            .generate_with_slug(input, "survey-my-thing", |_| Ok(false))
            .expect("collision lookup succeeds");
        assert!(
            id.starts_with("br-survey-my-thing-"),
            "expected prefix br-survey-my-thing-, got {id}"
        );
        let parsed = parse_id(&id).expect("slug-shaped ID should parse");
        assert_eq!(parsed.prefix, "br-survey-my-thing");
        assert!(!parsed.hash.is_empty(), "hash suffix must be present");

        // Empty slug → falls back to hash-only generation.
        let id = id_gen
            .generate_with_slug(input, "", |_| Ok(false))
            .expect("collision lookup succeeds");
        let parsed = parse_id(&id).expect("hash-only ID should parse");
        assert_eq!(parsed.prefix, "br");

        // Slug-only collision falls through hash-extension. Force exactly the
        // very first candidate to "exist" and confirm the generator still
        // produces a slug-shaped ID by retrying with a different nonce.
        let calls = std::cell::Cell::new(0u32);
        let id = id_gen
            .generate_with_slug(input, "survey-my-thing", |_candidate| {
                let n = calls.get();
                calls.set(n + 1);
                Ok(n == 0)
            })
            .expect("collision lookup succeeds");
        assert!(id.starts_with("br-survey-my-thing-"));
    }

    #[test]
    fn test_id_generator_generate_with_slug_respects_prefix_budget() {
        let prefix = "p".repeat(MAX_ID_PREFIX_LEN - 4);
        let id_gen = IdGenerator::new(IdConfig {
            prefix: prefix.clone(),
            ..IdConfig::default()
        });
        let id = id_gen
            .generate_with_slug(
                IdGenerationInput {
                    title: "title",
                    description: None,
                    creator: None,
                    created_at: Utc::now(),
                    issue_count: 10,
                },
                "abcd-efgh",
                |_| Ok(false),
            )
            .expect("collision lookup succeeds");

        let parsed = parse_id(&id).expect("slug-shaped ID should stay parseable");
        assert_eq!(parsed.prefix, format!("{prefix}-abc"));
        assert!(parsed.prefix.len() <= MAX_ID_PREFIX_LEN);
    }

    #[test]
    fn test_id_generator_generate_with_slug_falls_back_without_prefix_budget() {
        let prefix = "p".repeat(MAX_ID_PREFIX_LEN);
        let id_gen = IdGenerator::new(IdConfig {
            prefix: prefix.clone(),
            ..IdConfig::default()
        });
        let id = id_gen
            .generate_with_slug(
                IdGenerationInput {
                    title: "title",
                    description: None,
                    creator: None,
                    created_at: Utc::now(),
                    issue_count: 10,
                },
                "slug",
                |_| Ok(false),
            )
            .expect("collision lookup succeeds");

        let parsed = parse_id(&id).expect("fallback ID should stay parseable");
        assert_eq!(parsed.prefix, prefix);
    }

    // ========================================================================
    // beads_rust-l6xl: post-`--slug` audit unit tests (added 2026-05-09)
    // Per the bead's required NEW unit tests.
    // ========================================================================

    /// l6xl AC: `--slug "Hello World!"` → `<prefix>-hello-world-<hash>`.
    /// Asserts case-folding, single-hyphen collapsing, leading/trailing
    /// non-ASCII strip, and that the hash suffix is always present.
    #[test]
    fn create_with_slug_normalizes_to_ascii_lowercase_hyphenated() {
        let id_gen = IdGenerator::new(IdConfig::default());
        let input = IdGenerationInput {
            title: "Whatever",
            description: None,
            creator: None,
            created_at: Utc::now(),
            issue_count: 10,
        };

        // Mixed-case + spaces + punctuation → lowercased, single-hyphenated
        let id = id_gen
            .generate_with_slug(input, "Hello World!", |_| Ok(false))
            .expect("collision lookup succeeds");
        assert!(
            id.starts_with("br-hello-world-"),
            "expected prefix 'br-hello-world-', got {id}"
        );
        let parsed = parse_id(&id).expect("must parse");
        assert_eq!(parsed.prefix, "br-hello-world");
        assert!(!parsed.hash.is_empty(), "hash suffix must be present");

        // Multiple non-alphanumeric runs collapse to a single hyphen
        let id2 = id_gen
            .generate_with_slug(input, "a   b/c.d!!e", |_| Ok(false))
            .expect("collision lookup succeeds");
        assert!(
            id2.starts_with("br-a-b-c-d-e-"),
            "expected b-c-d-e collapsed; got {id2}"
        );

        // Pure-Unicode (non-ASCII) characters strip out
        let id3 = id_gen
            .generate_with_slug(input, "café-résumé", |_| Ok(false))
            .expect("collision lookup succeeds");
        // After normalize_slug: "caf-r-sum" (drops é, kept letters around hyphens)
        assert!(id3.starts_with("br-caf-r-sum-"), "got {id3}");
    }

    /// l6xl AC: a slug longer than `MAX_SLUG_LEN` (48 chars after
    /// normalization) is truncated, then any trailing hyphen is trimmed.
    #[test]
    fn create_with_slug_caps_at_48_chars_after_normalization() {
        let id_gen = IdGenerator::new(IdConfig::default());
        let input = IdGenerationInput {
            title: "Whatever",
            description: None,
            creator: None,
            created_at: Utc::now(),
            issue_count: 10,
        };

        // 60-char input → 48-char normalized cap (not counting prefix/hash)
        let long_slug = "a".repeat(60);
        let id = id_gen
            .generate_with_slug(input, &long_slug, |_| Ok(false))
            .expect("collision lookup succeeds");
        let parsed = parse_id(&id).expect("must parse");
        let slug_part = parsed.prefix.strip_prefix("br-").unwrap_or(&parsed.prefix);
        assert!(
            slug_part.len() <= MAX_SLUG_LEN,
            "slug {slug_part} exceeded MAX_SLUG_LEN ({MAX_SLUG_LEN}); got len={}",
            slug_part.len()
        );

        // Slug that ends with a hyphen due to truncation must have it stripped
        let trailing_hyphen_slug = format!("{}!!!", "x".repeat(MAX_SLUG_LEN - 2)); // becomes 48 chars then trailing hyphen risk
        let id2 = id_gen
            .generate_with_slug(input, &trailing_hyphen_slug, |_| Ok(false))
            .expect("collision lookup succeeds");
        assert!(!id2.starts_with("br--"), "double-hyphen leak in {id2}");
        // Find the slug portion: between "br-" and the last "-<hash>" segment
        let parsed2 = parse_id(&id2).expect("must parse");
        let slug2 = parsed2
            .prefix
            .strip_prefix("br-")
            .unwrap_or(&parsed2.prefix);
        assert!(
            !slug2.ends_with('-'),
            "slug retained trailing hyphen: {slug2}"
        );
    }

    /// l6xl AC: the configured prefix is preserved AND the hash suffix is
    /// always appended. Round-trips through `parse_id`.
    #[test]
    fn create_with_slug_preserves_configured_prefix_and_hash_suffix() {
        let id_gen = IdGenerator::new(IdConfig {
            prefix: "myproj".to_string(),
            ..IdConfig::default()
        });
        let input = IdGenerationInput {
            title: "Whatever",
            description: None,
            creator: None,
            created_at: Utc::now(),
            issue_count: 5,
        };

        let id = id_gen
            .generate_with_slug(input, "feature-x", |_| Ok(false))
            .expect("collision lookup succeeds");
        assert!(
            id.starts_with("myproj-feature-x-"),
            "expected configured prefix preserved; got {id}"
        );
        let parsed = parse_id(&id).expect("must parse");
        assert_eq!(parsed.prefix, "myproj-feature-x");
        assert!(!parsed.hash.is_empty(), "hash suffix must be present");
        // Hash suffix grows with issue_count; at least 3 chars at issue_count=5
        // (per IdGenerator::optimal_length)
        assert!(
            parsed.hash.len() >= 3,
            "hash suffix too short: {} (expected >= 3)",
            parsed.hash.len()
        );
    }

    /// l6xl AC: a slug like "!!!" or "...." that normalizes to empty
    /// MUST fall back to the hash-only ID path; it must NOT panic, error,
    /// or produce a malformed ID.
    #[test]
    fn create_with_slug_rejects_empty_after_normalization() {
        let id_gen = IdGenerator::new(IdConfig::default());
        let input = IdGenerationInput {
            title: "Title from fallback",
            description: None,
            creator: None,
            created_at: Utc::now(),
            issue_count: 10,
        };

        // Various inputs that all normalize to empty
        for empty_yielding in ["!!!", "...", "   ", "$%^&*()", "", "/\\/\\"] {
            let id = id_gen
                .generate_with_slug(input, empty_yielding, |_| Ok(false))
                .expect("collision lookup succeeds");
            let parsed = parse_id(&id).expect("must parse hash-only fallback");
            assert_eq!(
                parsed.prefix, "br",
                "empty-normalizing slug {empty_yielding:?} should fall back to hash-only; got {id}"
            );
            assert!(
                !parsed.hash.is_empty(),
                "hash-only fallback must include hash; got {id}"
            );
        }
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn templated_id_renders_sequence_slug_and_hash() {
        let id_gen = IdGenerator::new(IdConfig {
            generation: IdGenerationConfig {
                mode: IdGenerationMode::Templated,
                template: "{seq:03}-{slug}-{hash}".to_string(),
                require_slug: true,
                allow_non_unique_template: false,
            },
            ..IdConfig::default()
        });
        let id = id_gen
            .generate_with_template(
                IdGenerationInput {
                    title: "Improve tests speed",
                    description: None,
                    creator: None,
                    created_at: Utc::now(),
                    issue_count: 1,
                },
                Some(IssueSequenceNumber::new(1).unwrap()),
                Some("Improve tests speed"),
                |_| Ok(false),
            )
            .expect("templated ID");

        assert!(id.starts_with("001-improve-tests-speed-"), "got {id}");
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn templated_id_rejects_unknown_tokens_and_missing_hash_by_default() {
        let input = IdGenerationInput {
            title: "Title",
            description: None,
            creator: None,
            created_at: Utc::now(),
            issue_count: 1,
        };
        let unknown = IdGenerator::new(IdConfig {
            generation: IdGenerationConfig {
                mode: IdGenerationMode::Templated,
                template: "{seq:03}-{unknown}-{hash}".to_string(),
                require_slug: false,
                allow_non_unique_template: false,
            },
            ..IdConfig::default()
        });
        assert!(
            unknown
                .generate_with_template(
                    input,
                    Some(IssueSequenceNumber::new(1).unwrap()),
                    Some("slug"),
                    |_| Ok(false),
                )
                .is_err()
        );

        let unmatched_closing = IdGenerator::new(IdConfig {
            generation: IdGenerationConfig {
                mode: IdGenerationMode::Templated,
                template: "bad}{hash}".to_string(),
                require_slug: false,
                allow_non_unique_template: false,
            },
            ..IdConfig::default()
        });
        assert!(
            unmatched_closing
                .generate_with_template(input, None, None, |_| Ok(false))
                .is_err()
        );

        let invalid_shape = IdGenerator::new(IdConfig {
            generation: IdGenerationConfig {
                mode: IdGenerationMode::Templated,
                template: "{prefix}_{hash}".to_string(),
                require_slug: false,
                allow_non_unique_template: false,
            },
            ..IdConfig::default()
        });
        assert!(invalid_shape.validate_template_config().is_err());

        let missing_hash = IdGenerator::new(IdConfig {
            generation: IdGenerationConfig {
                mode: IdGenerationMode::Templated,
                template: "{seq:03}-{slug}".to_string(),
                require_slug: true,
                allow_non_unique_template: false,
            },
            ..IdConfig::default()
        });
        assert!(
            missing_hash
                .generate_with_template(
                    input,
                    Some(IssueSequenceNumber::new(1).unwrap()),
                    Some("slug"),
                    |_| Ok(false),
                )
                .is_err()
        );
    }

    #[test]
    #[allow(clippy::literal_string_with_formatting_args)]
    fn templated_id_allows_explicitly_unsafe_hashless_template() {
        let id_gen = IdGenerator::new(IdConfig {
            generation: IdGenerationConfig {
                mode: IdGenerationMode::Templated,
                template: "{seq:03}-{slug}".to_string(),
                require_slug: true,
                allow_non_unique_template: true,
            },
            ..IdConfig::default()
        });
        let id = id_gen
            .generate_with_template(
                IdGenerationInput {
                    title: "Unsafe template",
                    description: None,
                    creator: None,
                    created_at: Utc::now(),
                    issue_count: 1,
                },
                Some(IssueSequenceNumber::new(9).unwrap()),
                Some("Unsafe template"),
                |_| Ok(false),
            )
            .expect("explicit unsafe escape hatch");

        assert_eq!(id, "009-unsafe-template");
    }

    #[test]
    fn issue_sequence_number_is_positive_integer_on_the_wire() {
        let sequence = IssueSequenceNumber::new(42).expect("positive sequence");
        assert_eq!(serde_json::to_string(&sequence).unwrap(), "42");
        assert_eq!(
            serde_json::from_str::<IssueSequenceNumber>("42").unwrap(),
            sequence
        );
        for invalid in ["0", "-1"] {
            let error = serde_json::from_str::<IssueSequenceNumber>(invalid)
                .expect_err("non-positive sequence must be rejected");
            assert!(error.to_string().contains("expected positive integer"));
        }
    }

    #[test]
    fn exact_or_sequence_resolution_prefers_exact_and_rejects_zero() {
        let exact = resolve_exact_or_sequence_fallible(
            "001",
            "001",
            |candidate| Ok(candidate == "001"),
            |_| panic!("exact IDs must not consult sequence metadata"),
        )
        .unwrap();
        assert_eq!(exact.as_deref(), Some("001"));

        let error =
            resolve_exact_or_sequence_fallible("000", "000", |_| Ok(false), |_| Ok(Vec::new()))
                .expect_err("zero is not a valid issue sequence");
        assert!(error.to_string().contains("expected positive integer"));
    }

    #[test]
    fn issue_sequence_number_successor_fails_at_i64_max() {
        let maximum = IssueSequenceNumber::new(i64::MAX).expect("positive maximum");
        let error = maximum
            .checked_successor()
            .expect_err("maximum sequence has no successor");
        assert!(error.to_string().contains("sequence counter exhausted"));
    }
}
