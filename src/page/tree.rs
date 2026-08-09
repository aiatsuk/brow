//! The unified page tree and the `@node-N` reference model.
//!
//! One traversal produces a single node model that merges:
//!   * DOM structure, including **open and closed** shadow roots and nested
//!     iframes (`DOM.getDocument{pierce:true}` — verified 2026-08-04 to expose
//!     `mode:"closed"` roots, because CDP operates below the JS boundary),
//!   * accessibility role and name (`Accessibility.getFullAXTree`),
//!   * layout box, computed visibility and pointer eligibility
//!     (`DOMSnapshot.captureSnapshot`).
//!
//! References are `@node-G-N`, valid only for the *generation* and execution
//! target they were minted in. Navigating or changing the attached target tree
//! bumps `G`; every prior ref then becomes a loud error rather than a silent
//! mis-click.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cdp::{CdpClient, CdpError};

/// Axis-aligned box in CSS pixels, in the top-level viewport's coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// One node of the unified tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// Stable within a generation, e.g. `@node-42`.
    #[serde(rename = "ref")]
    pub node_ref: String,
    pub backend_node_id: i64,
    /// Renderer target and flat CDP session that own `backend_node_id`.
    /// Backend ids are not globally unique across OOPIF renderer processes.
    pub target_id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
    /// Has a non-degenerate layout box and is eligible for CSS pointer hit
    /// testing. Opacity is deliberately ignored: transparent native controls
    /// commonly provide real input beneath a styled checkbox. Occlusion is
    /// checked live immediately before an action.
    #[serde(default)]
    pub pointer_eligible: bool,
    /// Whether the node is perceptually painted. This can be false while
    /// `pointer_eligible` is true.
    pub visible: bool,
    pub disabled: bool,
    pub interactive: bool,
    pub depth: usize,
    /// True when the node lives inside a shadow root (open or closed).
    pub in_shadow: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_root_type: Option<String>,
}

/// A ref's immutable browser-side identity.
///
/// `backend_node_id` is meaningful only inside `session_id`; including the
/// execution target, frame and generation prevents an id collision or renderer
/// swap from turning an old approval/ref into a different live node.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub target_id: String,
    pub session_id: String,
    pub frame_id: String,
    pub backend_node_id: i64,
    pub generation: u64,
}

/// Opaque, per-Page keyed equality token for unredacted action semantics.
///
/// Debug deliberately reveals nothing and serde skips the field on the enclosing
/// fingerprint, preventing a low-entropy secret URL from becoming an offline hash
/// oracle in logs or manifests.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct ActionSemanticsToken(u64);

impl ActionSemanticsToken {
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }
}

impl std::fmt::Debug for ActionSemanticsToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[opaque]")
    }
}

/// Identity plus freshly-read semantics, used to revalidate an approved target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeFingerprint {
    pub identity: NodeIdentity,
    pub tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessible_label: Option<String>,
    /// Fresh action-relevant DOM attributes. These close the gap where the same
    /// backend node and label are retained but its destination or submit behavior
    /// changes while an approval is pending.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub action_attributes: BTreeMap<String, String>,
    /// Keyed equality token over unredacted raw + effective action semantics.
    #[serde(skip)]
    pub(crate) action_semantics_token: ActionSemanticsToken,
}

/// A renderer subtree that could not be observed completely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageGap {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    pub reason: String,
}

/// A whole-page capture plus the ref table it minted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub generation: u64,
    pub url: String,
    pub title: String,
    pub nodes: Vec<Node>,
    /// Never silently omit an unattached, failed, or racing renderer subtree.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage_gaps: Vec<CoverageGap>,
}

impl Snapshot {
    pub fn interactive(&self) -> impl Iterator<Item = &Node> {
        self.nodes
            .iter()
            .filter(|n| n.interactive && n.pointer_eligible)
    }

    /// Compact text rendering — the default output, tuned for an LLM reader.
    ///
    /// One line per node, no closing tags, no punctuation an agent has to parse.
    /// The full node record is available via `--json` when something specific is
    /// actually needed; dumping it for every node would cost thousands of tokens
    /// to say almost nothing.
    pub fn render_text(&self, interactive_only: bool) -> String {
        let mut out = String::new();
        out.push_str(&format!("{}  \"{}\"\n", self.url, self.title));
        out.push_str(&format!("generation {}\n\n", self.generation));

        for gap in &self.coverage_gaps {
            out.push_str("coverage_gap");
            if let Some(frame) = &gap.frame_id {
                out.push_str(&format!(" frame={frame}"));
            }
            if let Some(target) = &gap.target_id {
                out.push_str(&format!(" target={target}"));
            }
            out.push_str(&format!(" reason={}\n", truncate(&gap.reason, 160)));
        }
        if !self.coverage_gaps.is_empty() {
            out.push('\n');
        }

        let nodes: Vec<&Node> = if interactive_only {
            self.interactive().collect()
        } else {
            self.nodes
                .iter()
                .filter(|n| n.visible || (n.interactive && n.pointer_eligible))
                .collect()
        };

        if nodes.is_empty() {
            out.push_str("(no matching nodes)\n");
            return out;
        }

        for n in nodes {
            let indent = "  ".repeat(if interactive_only { 0 } else { n.depth.min(12) });
            out.push_str(&indent);
            out.push_str(&n.node_ref);
            out.push(' ');
            out.push_str(&n.tag);
            if let Some(role) = &n.role {
                if role != &n.tag {
                    out.push_str(&format!(" role={role}"));
                }
            }
            if let Some(name) = n.name.as_deref().filter(|s| !s.is_empty()) {
                out.push_str(&format!(" \"{}\"", truncate(name, 80)));
            } else if let Some(text) = n.text.as_deref().filter(|s| !s.is_empty()) {
                out.push_str(&format!(" \"{}\"", truncate(text, 80)));
            }
            for key in ["id", "name", "type", "placeholder", "href", "value"] {
                if let Some(v) = n.attrs.get(key) {
                    out.push_str(&format!(" {key}={}", truncate(v, 60)));
                }
            }
            if n.disabled {
                out.push_str(" disabled");
            }
            if !n.visible && n.pointer_eligible {
                out.push_str(" transparent");
            }
            if n.in_shadow {
                out.push_str(" shadow");
            }
            if let Some(b) = n.bounds {
                out.push_str(&format!(
                    " [{:.0},{:.0} {:.0}x{:.0}]",
                    b.x, b.y, b.width, b.height
                ));
            }
            out.push('\n');
        }
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        s
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Maps `@node-N` back to the browser-side node it was minted from.
#[derive(Debug, Default)]
pub struct RefTable {
    generation: u64,
    entries: HashMap<u64, RefEntry>,
    identity_sequences: HashMap<NodeIdentity, u64>,
}

#[derive(Debug, Clone)]
pub struct RefEntry {
    pub identity: NodeIdentity,
    pub tag: String,
    pub role: Option<String>,
    pub accessible_label: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RefError {
    #[error("{0} is not a node reference (expected something like @node-42)")]
    Malformed(String),
    #[error(
        "{node_ref} is stale: it was minted for generation {had}, the page is now at \
         generation {now}. Take a fresh `brow snapshot` and use the new refs."
    )]
    Stale {
        node_ref: String,
        had: u64,
        now: u64,
    },
    #[error("{0} is unknown — no such node in the current snapshot")]
    Unknown(String),
}

impl RefTable {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Invalidates every outstanding ref. Called when the document is replaced.
    ///
    /// Entries are deliberately *kept*, carrying the generation they were minted
    /// in. Dropping them would turn a stale ref into "unknown node", and the two
    /// need different advice: "unknown" means the agent invented a ref, "stale"
    /// means the page moved and it should re-snapshot. The table stays bounded
    /// because the next capture re-mints the same small integers and overwrites
    /// them.
    pub fn bump(&mut self) -> u64 {
        self.generation += 1;
        self.identity_sequences.clear();
        self.generation
    }

    /// Adopts the page's current generation.
    pub fn set_generation(&mut self, generation: u64) {
        if self.generation != generation {
            self.identity_sequences.clear();
        }
        self.generation = generation;
    }

    /// Starts a replacement snapshot while preserving refs for browser-side
    /// identities that are still present in the same document generation.
    pub fn next_snapshot(&self, generation: u64) -> Self {
        Self {
            generation,
            entries: HashMap::new(),
            identity_sequences: if self.generation == generation {
                self.identity_sequences.clone()
            } else {
                HashMap::new()
            },
        }
    }

    /// Forget identities absent from the completed snapshot. Their numeric refs
    /// are never reused because the Page counter is monotonic, but retaining every
    /// transient SPA node for an entire document generation would be unbounded.
    pub fn prune_absent_identities(&mut self) {
        let entries = &self.entries;
        self.identity_sequences
            .retain(|_, sequence| entries.contains_key(sequence));
    }

    #[cfg(test)]
    fn remembered_identity_count(&self) -> usize {
        self.identity_sequences.len()
    }

    fn mint(
        &mut self,
        counter: &mut u64,
        identity: NodeIdentity,
        tag: &str,
        role: Option<&str>,
        accessible_label: Option<&str>,
    ) -> String {
        let n = if let Some(sequence) = self.identity_sequences.get(&identity) {
            *sequence
        } else {
            *counter = counter.saturating_add(1);
            self.identity_sequences.insert(identity.clone(), *counter);
            *counter
        };
        self.entries.insert(
            n,
            RefEntry {
                identity,
                tag: tag.to_string(),
                role: role.map(str::to_string),
                accessible_label: accessible_label.map(str::to_string),
            },
        );
        format!("@node-{}-{n}", self.generation)
    }

    /// Resolves a ref against the page's *current* generation.
    ///
    /// The generation is passed in rather than read from the table because the two
    /// legitimately differ: the table is stamped when a snapshot is taken, and the
    /// page moves on afterwards. Comparing against the table's own generation
    /// would call every stale ref valid.
    pub fn resolve(&self, node_ref: &str, current: u64) -> Result<RefEntry, RefError> {
        let parsed =
            parse_ref_parts(node_ref).ok_or_else(|| RefError::Malformed(node_ref.to_string()))?;
        if let Some(had) = parsed.generation {
            if had != current {
                return Err(RefError::Stale {
                    node_ref: node_ref.to_string(),
                    had,
                    now: current,
                });
            }
        }
        let entry = self
            .entries
            .get(&parsed.sequence)
            .ok_or_else(|| RefError::Unknown(node_ref.to_string()))?;
        if entry.identity.generation != current {
            return Err(RefError::Stale {
                node_ref: node_ref.to_string(),
                had: entry.identity.generation,
                now: current,
            });
        }
        Ok(entry.clone())
    }
}

/// Accepts `@node-42`, `node-42`, `@42` and `42`.
pub fn parse_ref(s: &str) -> Option<u64> {
    parse_ref_parts(s).map(|p| p.sequence)
}

struct ParsedRef {
    generation: Option<u64>,
    sequence: u64,
}

fn parse_ref_parts(s: &str) -> Option<ParsedRef> {
    let s = s.trim();
    let s = s.strip_prefix('@').unwrap_or(s);
    let s = s.strip_prefix("node-").unwrap_or(s);
    if let Some((generation, sequence)) = s.split_once('-') {
        return Some(ParsedRef {
            generation: Some(generation.parse().ok()?),
            sequence: sequence.parse().ok()?,
        });
    }
    Some(ParsedRef {
        generation: None,
        sequence: s.parse().ok()?,
    })
}

/// Tags that are interactive regardless of what the accessibility tree thinks.
const INTERACTIVE_TAGS: &[&str] = &[
    "a", "button", "input", "select", "textarea", "summary", "option", "details",
];

/// Roles that mean "a user can act on this".
const INTERACTIVE_ROLES: &[&str] = &[
    "button",
    "link",
    "checkbox",
    "radio",
    "textbox",
    "combobox",
    "listbox",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "searchbox",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "treeitem",
    "gridcell",
    "columnheader",
];

fn is_interactive(tag: &str, role: Option<&str>, attrs: &BTreeMap<String, String>) -> bool {
    if INTERACTIVE_TAGS.contains(&tag) {
        return true;
    }
    if let Some(role) = role {
        if INTERACTIVE_ROLES.contains(&role) {
            return true;
        }
    }
    if attrs.contains_key("onclick") {
        return true;
    }
    if attrs.get("contenteditable").is_some_and(|v| v != "false") {
        return true;
    }
    // tabindex="-1" is programmatic focus only, not a user affordance.
    if let Some(ti) = attrs.get("tabindex") {
        if ti.parse::<i32>().map(|v| v >= 0).unwrap_or(false) {
            return true;
        }
    }
    false
}

/// Affine map from one renderer target's viewport to the top-level viewport.
/// The router derives this from each owner iframe's content quad.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewportTransform {
    pub xx: f64,
    pub xy: f64,
    pub yx: f64,
    pub yy: f64,
    pub tx: f64,
    pub ty: f64,
}

impl ViewportTransform {
    pub const IDENTITY: Self = Self {
        xx: 1.0,
        xy: 0.0,
        yx: 0.0,
        yy: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub(crate) fn point(self, x: f64, y: f64) -> (f64, f64) {
        (
            self.tx + self.xx * x + self.xy * y,
            self.ty + self.yx * x + self.yy * y,
        )
    }

    fn bounds(self, b: Bounds) -> Bounds {
        let corners = [
            self.point(b.x, b.y),
            self.point(b.x + b.width, b.y),
            self.point(b.x + b.width, b.y + b.height),
            self.point(b.x, b.y + b.height),
        ];
        let min_x = corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let min_y = corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let max_x = corners
            .iter()
            .map(|p| p.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let max_y = corners
            .iter()
            .map(|p| p.1)
            .fold(f64::NEG_INFINITY, f64::max);
        Bounds {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        }
    }
}

pub(crate) struct CaptureContext<'a> {
    pub target_id: &'a str,
    pub session_id: &'a str,
    pub root_frame_id: &'a str,
    pub generation: u64,
    pub transform: ViewportTransform,
    /// The DOMSnapshot layout is document-relative; refs/actions use viewport
    /// coordinates, so remove the renderer target's current scroll offset first.
    pub page_x: f64,
    pub page_y: f64,
    pub depth_offset: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ExternalFrame {
    pub frame_id: String,
    pub owner_backend_node_id: i64,
}

pub(crate) struct CapturedTarget {
    pub url: String,
    pub title: String,
    pub nodes: Vec<Node>,
    pub external_frames: Vec<ExternalFrame>,
    /// Explicit degradation notices for optional enrichment. Callers surface
    /// these as coverage gaps instead of silently presenting a complete tree.
    pub warnings: Vec<String>,
}

/// Captures one renderer target. `Page` calls this once per recursively attached
/// OOPIF session and joins the returned fragments.
pub(crate) async fn capture_target(
    client: &CdpClient,
    context: &CaptureContext<'_>,
    refs: &mut RefTable,
    counter: &mut u64,
) -> Result<CapturedTarget, CdpError> {
    // These are idempotent; re-enabling on every snapshot keeps us correct after a
    // renderer swap without tracking per-target enable state.
    client
        .call_on(context.session_id, "DOM.enable", json!({}))
        .await?;
    let accessibility_enable = client
        .call_on(context.session_id, "Accessibility.enable", json!({}))
        .await;

    let doc = client
        .call_on(
            context.session_id,
            "DOM.getDocument",
            json!({ "depth": -1, "pierce": true }),
        )
        .await?;

    let frames = frame_ids(client, context.session_id).await?;
    let (ax, mut warnings) = ax_index(client, context.session_id, &frames).await;
    if let Err(error) = accessibility_enable {
        warnings.push(format!("Accessibility.enable failed: {error}"));
    }
    let layout = layout_index(client, context.session_id).await?;

    let mut nodes = Vec::new();
    let mut external_frames = Vec::new();
    if let Some(root) = doc.get("root") {
        walk(
            root,
            context.depth_offset,
            false,
            true,
            Some(context.root_frame_id),
            (0.0, 0.0),
            context,
            refs,
            &ax,
            &layout,
            counter,
            &mut nodes,
            &mut external_frames,
        );
    }

    let url = doc
        .get("root")
        .and_then(|r| r.get("documentURL"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let title = nodes
        .iter()
        .find(|n| n.tag == "title")
        .and_then(|n| n.text.clone())
        .unwrap_or_default();

    Ok(CapturedTarget {
        url,
        title,
        nodes,
        external_frames,
        warnings,
    })
}

/// Every frame in this session's tree, main frame first.
async fn frame_ids(client: &CdpClient, session_id: &str) -> Result<Vec<String>, CdpError> {
    let tree = client
        .call_on(session_id, "Page.getFrameTree", json!({}))
        .await?;
    let mut out = Vec::new();
    fn walk_frames(node: &Value, out: &mut Vec<String>) {
        if let Some(id) = node
            .get("frame")
            .and_then(|f| f.get("id"))
            .and_then(Value::as_str)
        {
            out.push(id.to_string());
        }
        for child in node
            .get("childFrames")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            walk_frames(child, out);
        }
    }
    if let Some(root) = tree.get("frameTree") {
        walk_frames(root, &mut out);
    }
    Ok(out)
}

/// backendDOMNodeId -> (role, accessible name), across every frame.
///
/// `Accessibility.getFullAXTree` does **not** cross iframe boundaries — not even
/// same-origin ones. Measured 2026-08-04: a `<button aria-label="Close dialog">`
/// with no text content, inside a same-origin iframe, came back from a single
/// whole-page call with no role and no name at all, so it rendered as a bare
/// `button` that an agent could not identify. The fix is one call per frame,
/// merged on `backendDOMNodeId`, which is unique across the whole session.
///
/// Out-of-process iframes still need their own attached session and are not
/// covered here.
async fn ax_index(
    client: &CdpClient,
    session_id: &str,
    frames: &[String],
) -> (HashMap<i64, (String, String)>, Vec<String>) {
    let mut out = HashMap::new();
    let mut warnings = Vec::new();

    // An empty frame list still gets the default whole-session call, so a failure
    // to read the frame tree degrades rather than blanks the accessibility data.
    let calls: Vec<Value> = if frames.is_empty() {
        vec![json!({})]
    } else {
        frames.iter().map(|id| json!({ "frameId": id })).collect()
    };

    for params in calls {
        let tree = match client
            .call_on(session_id, "Accessibility.getFullAXTree", params)
            .await
        {
            Ok(tree) => tree,
            Err(error) => {
                // Accessibility is enrichment, never a hard dependency: a page tree
                // without roles is degraded, not broken, but it is never silent.
                warnings.push(format!("Accessibility.getFullAXTree failed: {error}"));
                continue;
            }
        };
        let Some(list) = tree.get("nodes").and_then(Value::as_array) else {
            continue;
        };
        for n in list {
            let Some(backend) = n.get("backendDOMNodeId").and_then(Value::as_i64) else {
                continue;
            };
            if n.get("ignored").and_then(Value::as_bool).unwrap_or(false) {
                continue;
            }
            let role = n
                .get("role")
                .and_then(|r| r.get("value"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let name = n
                .get("name")
                .and_then(|r| r.get("value"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            out.insert(backend, (role, name));
        }
    }
    (out, warnings)
}

struct LayoutInfo {
    bounds: Bounds,
    visible: bool,
    pointer_eligible: bool,
    opacity_nonzero: bool,
}

/// backendNodeId -> layout box, computed visibility and pointer eligibility.
///
/// `DOMSnapshot.captureSnapshot` returns one flattened record for the entire page
/// (all documents) in a single round trip. Nodes with `display:none` have no
/// layout object at all and are therefore simply absent — which is exactly the
/// visibility signal we want, for free.
async fn layout_index(
    client: &CdpClient,
    session_id: &str,
) -> Result<HashMap<i64, LayoutInfo>, CdpError> {
    const STYLES: [&str; 3] = ["visibility", "opacity", "pointer-events"];
    let mut out = HashMap::new();

    let snap = client
        .call_on(
            session_id,
            "DOMSnapshot.captureSnapshot",
            json!({
                "computedStyles": STYLES,
                "includeDOMRects": false,
                "includePaintOrder": false,
            }),
        )
        .await?;

    let strings: Vec<&str> = snap
        .get("strings")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|v| v.as_str().unwrap_or_default()).collect())
        .unwrap_or_default();
    let lookup = |idx: i64| -> &str {
        usize::try_from(idx)
            .ok()
            .and_then(|i| strings.get(i).copied())
            .unwrap_or_default()
    };

    let Some(documents) = snap.get("documents").and_then(Value::as_array) else {
        return Ok(out);
    };

    for doc in documents {
        let backend_ids: Vec<i64> = doc
            .get("nodes")
            .and_then(|n| n.get("backendNodeId"))
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| v.as_i64().unwrap_or(-1)).collect())
            .unwrap_or_default();

        let Some(layout) = doc.get("layout") else {
            continue;
        };
        let node_index: Vec<i64> = layout
            .get("nodeIndex")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|v| v.as_i64().unwrap_or(-1)).collect())
            .unwrap_or_default();
        let empty = Vec::new();
        let boxes = layout
            .get("bounds")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        let styles = layout.get("styles").and_then(Value::as_array);

        for (i, &node_idx) in node_index.iter().enumerate() {
            let Some(&backend) = usize::try_from(node_idx)
                .ok()
                .and_then(|i| backend_ids.get(i))
            else {
                continue;
            };
            let Some(rect) = boxes.get(i).and_then(Value::as_array) else {
                continue;
            };
            if rect.len() < 4 {
                continue;
            }
            let bounds = Bounds {
                x: rect[0].as_f64().unwrap_or(0.0),
                y: rect[1].as_f64().unwrap_or(0.0),
                width: rect[2].as_f64().unwrap_or(0.0),
                height: rect[3].as_f64().unwrap_or(0.0),
            };

            // `styles[i]` is one string-index per entry of STYLES, in order.
            // Opacity affects painting but not pointer hit testing: TodoMVC-style
            // checkboxes intentionally use an opacity-zero native input beneath
            // their visible CSS decoration.
            let has_box = bounds.width > 0.0 && bounds.height > 0.0;
            let mut style_visible = true;
            let mut opacity_nonzero = true;
            let mut pointer_events = true;
            if let Some(row) = styles.and_then(|s| s.get(i)).and_then(Value::as_array) {
                if let Some(v) = row.first().and_then(Value::as_i64) {
                    style_visible = !matches!(lookup(v), "hidden" | "collapse");
                }
                if let Some(v) = row.get(1).and_then(Value::as_i64) {
                    opacity_nonzero = lookup(v)
                        .parse::<f64>()
                        .map(|opacity| opacity > 0.0)
                        .unwrap_or(true);
                }
                if let Some(v) = row.get(2).and_then(Value::as_i64) {
                    pointer_events = lookup(v) != "none";
                }
            }

            out.insert(
                backend,
                LayoutInfo {
                    bounds,
                    visible: has_box && style_visible && opacity_nonzero,
                    pointer_eligible: has_box && style_visible && pointer_events,
                    opacity_nonzero,
                },
            );
        }
    }

    Ok(out)
}

/// Walks the pierced DOM, minting refs and joining in accessibility and layout.
///
/// `offset` is the document-space origin of the document being walked. It is
/// non-zero inside an iframe: `DOMSnapshot.captureSnapshot` returns one record per
/// document and each one's layout boxes are **frame-local**, so a button at (8,8)
/// inside an iframe positioned at (30,0) is reported at (8,8) unless we add the
/// offset back. `DOM.getContentQuads`, which is what input and screenshots use,
/// is already global — so before this was fixed, `bounds` and the actual click
/// point disagreed for everything inside a frame.
#[allow(clippy::too_many_arguments)]
fn walk(
    node: &Value,
    depth: usize,
    in_shadow: bool,
    ancestor_opacity_nonzero: bool,
    inherited_frame: Option<&str>,
    offset: (f64, f64),
    context: &CaptureContext<'_>,
    refs: &mut RefTable,
    ax: &HashMap<i64, (String, String)>,
    layout: &HashMap<i64, LayoutInfo>,
    counter: &mut u64,
    out: &mut Vec<Node>,
    external_frames: &mut Vec<ExternalFrame>,
) {
    let node_type = node.get("nodeType").and_then(Value::as_i64).unwrap_or(0);
    let backend_node_id = node
        .get("backendNodeId")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    let reported_frame = node.get("frameId").and_then(Value::as_str);
    // On an <iframe>, `frameId` identifies the *owned child*, while the iframe
    // element and its backend id belong to the containing document. Prefer the
    // inherited document identity for every element; pass the reported child id
    // only when descending into an inline same-process contentDocument.
    let execution_frame = inherited_frame
        .or(reported_frame)
        .unwrap_or(context.root_frame_id)
        .to_string();
    let local_opacity_nonzero = layout
        .get(&backend_node_id)
        .map(|info| info.opacity_nonzero)
        .unwrap_or(true);
    let effective_opacity_nonzero = ancestor_opacity_nonzero && local_opacity_nonzero;

    // Element nodes only; text is folded into its parent below.
    if node_type == 1 && backend_node_id >= 0 {
        let tag = node
            .get("nodeName")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_lowercase();

        let mut attrs = BTreeMap::new();
        if let Some(list) = node.get("attributes").and_then(Value::as_array) {
            for pair in list.chunks(2) {
                if let [k, v] = pair {
                    if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                        attrs.insert(k.to_string(), v.to_string());
                    }
                }
            }
        }

        let text = direct_text(node);
        let (role, name) = ax
            .get(&backend_node_id)
            .map(|(r, n)| (Some(r.clone()), Some(n.clone())))
            .unwrap_or((None, None));
        let role = role.filter(|r| !r.is_empty());
        let name = name.filter(|n| !n.is_empty());
        let li = layout.get(&backend_node_id);
        let disabled = attrs.contains_key("disabled")
            || attrs.get("aria-disabled").is_some_and(|v| v == "true");

        let identity = NodeIdentity {
            target_id: context.target_id.to_string(),
            session_id: context.session_id.to_string(),
            frame_id: execution_frame.clone(),
            backend_node_id,
            generation: context.generation,
        };
        let node_ref = refs.mint(counter, identity, &tag, role.as_deref(), name.as_deref());

        out.push(Node {
            node_ref,
            backend_node_id,
            target_id: context.target_id.to_string(),
            session_id: context.session_id.to_string(),
            frame_id: Some(execution_frame.clone()),
            interactive: is_interactive(&tag, role.as_deref(), &attrs) && !disabled,
            tag,
            role,
            name,
            text,
            attrs,
            bounds: li.map(|l| {
                context.transform.bounds(Bounds {
                    x: l.bounds.x + offset.0 - context.page_x,
                    y: l.bounds.y + offset.1 - context.page_y,
                    ..l.bounds
                })
            }),
            pointer_eligible: li.map(|l| l.pointer_eligible).unwrap_or(false),
            visible: li
                .map(|l| l.visible && ancestor_opacity_nonzero)
                .unwrap_or(false),
            disabled,
            depth,
            in_shadow,
            shadow_root_type: node
                .get("shadowRootType")
                .and_then(Value::as_str)
                .map(str::to_string),
        });

        if node
            .get("nodeName")
            .and_then(Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("iframe"))
            && node.get("contentDocument").is_none()
        {
            if let Some(frame_id) = reported_frame {
                external_frames.push(ExternalFrame {
                    frame_id: frame_id.to_string(),
                    owner_backend_node_id: backend_node_id,
                });
            }
        }
    }

    let next_depth = if node_type == 1 { depth + 1 } else { depth };

    for child in node
        .get("children")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        walk(
            child,
            next_depth,
            in_shadow,
            effective_opacity_nonzero,
            Some(&execution_frame),
            offset,
            context,
            refs,
            ax,
            layout,
            counter,
            out,
            external_frames,
        );
    }
    // Shadow roots — open *and* closed, because CDP sees below the JS boundary.
    for root in node
        .get("shadowRoots")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        walk(
            root,
            next_depth,
            true,
            effective_opacity_nonzero,
            Some(&execution_frame),
            offset,
            context,
            refs,
            ax,
            layout,
            counter,
            out,
            external_frames,
        );
    }
    // Nested documents: same-process iframes arrive inline here. Out-of-process
    // iframes do not, and need their own attached session.
    if let Some(content) = node.get("contentDocument") {
        // The child document's origin is wherever this iframe element sits, in
        // coordinates we have already made global. Border and padding are not
        // accounted for, so a frame with a thick border is off by that much —
        // small, and only ever affects the informational `bounds` field, never
        // the click point.
        let child_offset = layout
            .get(&backend_node_id)
            .map(|l| (l.bounds.x + offset.0, l.bounds.y + offset.1))
            .unwrap_or(offset);
        walk(
            content,
            next_depth,
            in_shadow,
            effective_opacity_nonzero,
            reported_frame.or(Some(&execution_frame)),
            child_offset,
            context,
            refs,
            ax,
            layout,
            counter,
            out,
            external_frames,
        );
    }
}

/// Concatenates the immediate text-node children of an element.
fn direct_text(node: &Value) -> Option<String> {
    let children = node.get("children").and_then(Value::as_array)?;
    let mut buf = String::new();
    for c in children {
        if c.get("nodeType").and_then(Value::as_i64) == Some(3) {
            if let Some(v) = c.get("nodeValue").and_then(Value::as_str) {
                buf.push_str(v);
            }
        }
    }
    let trimmed = buf.split_whitespace().collect::<Vec<_>>().join(" ");
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_syntax_is_forgiving() {
        for form in ["@node-42", "node-42", "@42", "42", " @node-42 "] {
            assert_eq!(parse_ref(form), Some(42), "failed on {form:?}");
        }
        assert_eq!(parse_ref("@node-7-42"), Some(42));
        assert_eq!(parse_ref("@node-"), None);
        assert_eq!(parse_ref("button"), None);
    }

    #[test]
    fn stale_refs_fail_loudly_after_a_generation_bump() {
        let mut refs = RefTable::default();
        let mut counter = 0;
        let r = refs.mint(
            &mut counter,
            NodeIdentity {
                target_id: "target".into(),
                session_id: "session".into(),
                frame_id: "frame".into(),
                backend_node_id: 900,
                generation: 0,
            },
            "button",
            Some("button"),
            Some("Create"),
        );
        assert_eq!(
            refs.resolve(&r, refs.generation())
                .unwrap()
                .identity
                .backend_node_id,
            900
        );

        let now = refs.bump();
        let err = refs.resolve(&r, now).unwrap_err();
        // The message has to tell the agent what to *do*, not just that it failed.
        let msg = err.to_string();
        assert!(msg.contains("stale"), "{msg}");
        assert!(msg.contains("brow snapshot"), "{msg}");
    }

    #[test]
    fn repeated_snapshot_reuses_ref_only_for_the_same_identity() {
        let identity = NodeIdentity {
            target_id: "target".into(),
            session_id: "session".into(),
            frame_id: "frame".into(),
            backend_node_id: 7,
            generation: 4,
        };
        let mut counter = 0;
        let mut first = RefTable::default();
        first.set_generation(4);
        let original = first.mint(
            &mut counter,
            identity.clone(),
            "button",
            Some("button"),
            Some("Go"),
        );

        let mut second = first.next_snapshot(4);
        let stable = second.mint(&mut counter, identity, "button", Some("button"), Some("Go"));
        let different = second.mint(
            &mut counter,
            NodeIdentity {
                target_id: "target".into(),
                session_id: "session".into(),
                frame_id: "frame".into(),
                backend_node_id: 8,
                generation: 4,
            },
            "button",
            Some("button"),
            Some("Other"),
        );
        assert_eq!(stable, original);
        assert_ne!(different, original);
    }

    #[test]
    fn same_generation_dom_churn_does_not_grow_identity_memory() {
        let mut table = RefTable::default();
        table.set_generation(9);
        let mut counter = 0;
        for backend_node_id in 1..=1_000 {
            let mut next = table.next_snapshot(9);
            next.mint(
                &mut counter,
                NodeIdentity {
                    target_id: "target".into(),
                    session_id: "session".into(),
                    frame_id: "frame".into(),
                    backend_node_id,
                    generation: 9,
                },
                "button",
                Some("button"),
                Some("changing"),
            );
            next.prune_absent_identities();
            assert_eq!(next.remembered_identity_count(), 1);
            table = next;
        }
        assert_eq!(counter, 1_000, "removed identities must never recycle refs");
    }

    #[test]
    fn unknown_and_malformed_refs_are_distinguished() {
        let refs = RefTable::default();
        assert!(matches!(
            refs.resolve("@node-7", 0),
            Err(RefError::Unknown(_))
        ));
        assert!(matches!(
            refs.resolve("nope", 0),
            Err(RefError::Malformed(_))
        ));
    }

    #[test]
    fn interactivity_covers_tags_roles_and_handlers() {
        let mut attrs = BTreeMap::new();
        assert!(is_interactive("button", None, &attrs));
        assert!(is_interactive("div", Some("checkbox"), &attrs));
        assert!(!is_interactive("div", Some("presentation"), &attrs));

        attrs.insert("tabindex".into(), "0".into());
        assert!(is_interactive("div", None, &attrs));
        attrs.insert("tabindex".into(), "-1".into());
        assert!(
            !is_interactive("div", None, &attrs),
            "tabindex=-1 is programmatic focus, not a user affordance"
        );

        attrs.clear();
        attrs.insert("contenteditable".into(), "true".into());
        assert!(is_interactive("div", None, &attrs));
        attrs.insert("contenteditable".into(), "false".into());
        assert!(!is_interactive("div", None, &attrs));
    }

    #[test]
    fn walk_folds_text_into_its_element_and_pierces_shadow() {
        let doc = json!({
            "nodeType": 9, "backendNodeId": 1, "nodeName": "#document",
            "documentURL": "http://x/",
            "children": [{
                "nodeType": 1, "backendNodeId": 2, "nodeName": "BUTTON",
                "attributes": ["id", "go"],
                "children": [{ "nodeType": 3, "backendNodeId": 3, "nodeValue": "  Create\n account " }],
                "shadowRoots": [{
                    "nodeType": 11, "backendNodeId": 4, "nodeName": "#document-fragment",
                    "shadowRootType": "closed",
                    "children": [{
                        "nodeType": 1, "backendNodeId": 5, "nodeName": "SPAN",
                        "children": [{ "nodeType": 3, "backendNodeId": 6, "nodeValue": "inside closed" }]
                    }]
                }]
            }]
        });

        let mut refs = RefTable::default();
        let mut nodes = Vec::new();
        let mut external_frames = Vec::new();
        let mut counter = 0;
        let context = CaptureContext {
            target_id: "target",
            session_id: "session",
            root_frame_id: "frame",
            generation: 0,
            transform: ViewportTransform::IDENTITY,
            page_x: 0.0,
            page_y: 0.0,
            depth_offset: 0,
        };
        walk(
            &doc,
            0,
            false,
            true,
            Some("frame"),
            (0.0, 0.0),
            &context,
            &mut refs,
            &HashMap::new(),
            &HashMap::new(),
            &mut counter,
            &mut nodes,
            &mut external_frames,
        );

        assert_eq!(nodes.len(), 2, "two elements: button and the shadow span");
        let button = &nodes[0];
        assert_eq!(button.tag, "button");
        assert_eq!(button.text.as_deref(), Some("Create account"));
        assert_eq!(button.attrs.get("id").map(String::as_str), Some("go"));
        assert!(button.interactive);
        assert!(!button.in_shadow);

        let span = &nodes[1];
        assert!(
            span.in_shadow,
            "a closed shadow root's contents must be reachable"
        );
        assert_eq!(span.text.as_deref(), Some("inside closed"));
    }

    #[test]
    fn text_rendering_stays_compact() {
        let snap = Snapshot {
            generation: 3,
            url: "http://x/".into(),
            title: "T".into(),
            nodes: vec![Node {
                node_ref: "@node-1".into(),
                backend_node_id: 2,
                target_id: "target".into(),
                session_id: "session".into(),
                frame_id: None,
                tag: "button".into(),
                role: Some("button".into()),
                name: Some("Create account".into()),
                text: None,
                attrs: BTreeMap::from([("id".into(), "go".into())]),
                bounds: Some(Bounds {
                    x: 420.0,
                    y: 610.0,
                    width: 220.0,
                    height: 48.0,
                }),
                pointer_eligible: true,
                visible: true,
                disabled: false,
                interactive: true,
                depth: 2,
                in_shadow: false,
                shadow_root_type: None,
            }],
            coverage_gaps: Vec::new(),
        };
        let text = snap.render_text(true);
        assert!(text.contains("generation 3"));
        let line = text.lines().last().unwrap();
        assert_eq!(
            line,
            "@node-1 button \"Create account\" id=go [420,610 220x48]"
        );
    }

    #[test]
    fn transparent_pointer_target_stays_in_compact_snapshot() {
        let snap = Snapshot {
            generation: 4,
            url: "http://x/".into(),
            title: "T".into(),
            nodes: vec![Node {
                node_ref: "@node-2".into(),
                backend_node_id: 3,
                target_id: "target".into(),
                session_id: "session".into(),
                frame_id: None,
                tag: "input".into(),
                role: Some("checkbox".into()),
                name: Some("Toggle Todo".into()),
                text: None,
                attrs: BTreeMap::new(),
                bounds: Some(Bounds {
                    x: 10.0,
                    y: 20.0,
                    width: 40.0,
                    height: 40.0,
                }),
                pointer_eligible: true,
                visible: false,
                disabled: false,
                interactive: true,
                depth: 2,
                in_shadow: false,
                shadow_root_type: None,
            }],
            coverage_gaps: Vec::new(),
        };

        assert_eq!(snap.interactive().count(), 1);
        assert_eq!(
            snap.render_text(true).lines().last().unwrap(),
            "@node-2 input role=checkbox \"Toggle Todo\" transparent [10,20 40x40]"
        );
        assert!(snap.render_text(false).contains("@node-2"));
    }

    #[test]
    fn truncation_collapses_whitespace() {
        assert_eq!(truncate("a\n  b   c", 80), "a b c");
        assert_eq!(truncate("abcdef", 4), "abc…");
    }
}
