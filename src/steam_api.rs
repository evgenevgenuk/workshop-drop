//! Steam Web API (no key needed) and Workshop page parsing.

use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

const DETAILS_URL: &str = "https://api.steampowered.com/ISteamRemoteStorage/GetPublishedFileDetails/v1/";
const COLLECTION_URL: &str = "https://api.steampowered.com/ISteamRemoteStorage/GetCollectionDetails/v1/";

/// Extracts a Workshop item id from a link or a plain number.
pub fn parse_workshop_id(input: &str) -> Option<u64> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse::<u64>().ok().filter(|&v| v > 0);
    }
    let lower = s.to_ascii_lowercase();
    if !lower.contains("steamcommunity.com/") {
        return None;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[?&]id=(\d+)(?:[&#]|$)").unwrap());
    re.captures(&lower)?
        .get(1)?
        .as_str()
        .parse::<u64>()
        .ok()
        .filter(|&v| v > 0)
}

#[derive(Debug, Clone, Default)]
pub struct ItemDetails {
    pub id: u64,
    pub result: i64,
    pub title: String,
    pub tags: Vec<String>,
    pub file_size: u64,
    pub consumer_app_id: u64,
}

impl ItemDetails {
    pub fn is_map(&self) -> bool {
        self.tags.iter().any(|t| t.eq_ignore_ascii_case("map"))
    }
}

/// Collection child: (id, filetype). filetype 2 is a nested collection.
pub type CollectionChild = (u64, u64);

fn val_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn val_i64(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

pub struct SteamApi {
    http: reqwest::blocking::Client,
}

impl SteamApi {
    pub fn new() -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            // steamcommunity.com answers 429 to some "unusual" User-Agents.
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
            .build()
            .context("cannot create HTTP client")?;
        Ok(Self { http })
    }

    fn post_ids(&self, url: &str, count_key: &str, ids: &[u64]) -> Result<Value> {
        let mut form: Vec<(String, String)> = vec![(count_key.to_string(), ids.len().to_string())];
        for (i, id) in ids.iter().enumerate() {
            form.push((format!("publishedfileids[{i}]"), id.to_string()));
        }
        let resp = self
            .http
            .post(url)
            .form(&form)
            .send()
            .context("no response from api.steampowered.com")?
            .error_for_status()
            .context("Steam API returned an error")?;
        resp.json::<Value>().context("Steam API returned an unexpected response")
    }

    /// GetPublishedFileDetails for a list of ids (100 per request).
    pub fn get_details(&self, ids: &[u64]) -> Result<Vec<ItemDetails>> {
        let mut out = Vec::new();
        for chunk in ids.chunks(100) {
            let v = self.post_ids(DETAILS_URL, "itemcount", chunk)?;
            let arr = v["response"]["publishedfiledetails"]
                .as_array()
                .ok_or_else(|| anyhow!("no publishedfiledetails in Steam response"))?;
            for item in arr {
                let Some(id) = val_u64(&item["publishedfileid"]) else { continue };
                out.push(ItemDetails {
                    id,
                    result: val_i64(&item["result"]).unwrap_or(0),
                    title: item["title"].as_str().unwrap_or("").trim().to_string(),
                    tags: item["tags"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|t| t["tag"].as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                    file_size: val_u64(&item["file_size"]).unwrap_or(0),
                    consumer_app_id: val_u64(&item["consumer_app_id"]).unwrap_or(0),
                });
            }
        }
        Ok(out)
    }

    /// GetCollectionDetails: for every id that is a collection, returns its children.
    pub fn get_collection_children(&self, ids: &[u64]) -> Result<HashMap<u64, Vec<CollectionChild>>> {
        let mut out = HashMap::new();
        for chunk in ids.chunks(100) {
            let v = self.post_ids(COLLECTION_URL, "collectioncount", chunk)?;
            let Some(arr) = v["response"]["collectiondetails"].as_array() else { continue };
            for c in arr {
                let Some(id) = val_u64(&c["publishedfileid"]) else { continue };
                if val_i64(&c["result"]) != Some(1) {
                    continue;
                }
                let children: Vec<CollectionChild> = c["children"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|ch| Some((val_u64(&ch["publishedfileid"])?, val_u64(&ch["filetype"]).unwrap_or(0))))
                            .collect()
                    })
                    .unwrap_or_default();
                if !children.is_empty() {
                    out.insert(id, children);
                }
            }
        }
        Ok(out)
    }

    /// HTML of the item's Workshop page.
    pub fn fetch_page(&self, id: u64) -> Result<String> {
        let url = format!("https://steamcommunity.com/sharedfiles/filedetails/?id={id}");
        // Steam rate-limits (429): wait and retry.
        for attempt in 1..=4u64 {
            let resp = self.http.get(&url).send().context("Workshop page did not respond")?;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt < 4 {
                std::thread::sleep(Duration::from_secs(3 * attempt));
                continue;
            }
            return Ok(resp.error_for_status()?.text()?);
        }
        Err(anyhow!("Steam rate limit (429)"))
    }
}

#[derive(Debug, PartialEq)]
pub enum RequiredItems {
    /// The page has no "Required items" block.
    None,
    /// Found dependencies: (id, name).
    Found(Vec<(u64, String)>),
    /// The block exists but could not be parsed.
    Unparsed,
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#039;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
}

/// Finds the `requiredItemsContainer` block and the `filedetails/?id=` links inside it.
pub fn parse_required_items(html: &str, self_id: u64) -> RequiredItems {
    let Some(pos) = html.find("requiredItemsContainer") else {
        return RequiredItems::None;
    };
    let rest = &html[pos..];

    // The block ends at the first `</div>` that is not followed by `</a>`
    // (each item is `<a ...><div class="requiredItem">Name</div></a>`).
    static END_RE: OnceLock<Regex> = OnceLock::new();
    let end_re = END_RE.get_or_init(|| Regex::new(r"(?s)</div>\s*(</a>)?").unwrap());
    let mut end = rest.len().min(50_000);
    for c in end_re.captures_iter(rest) {
        if c.get(1).is_none() {
            end = c.get(0).unwrap().end();
            break;
        }
    }
    let block = &rest[..end.min(rest.len())];

    static ITEM_RE: OnceLock<Regex> = OnceLock::new();
    let item_re = ITEM_RE.get_or_init(|| {
        Regex::new(r#"(?s)filedetails/\?id=(\d+)[^>]*>\s*(?:<div[^>]*requiredItem[^>]*>\s*([^<]*))?"#).unwrap()
    });
    let mut items: Vec<(u64, String)> = Vec::new();
    for c in item_re.captures_iter(block) {
        let Ok(id) = c[1].parse::<u64>() else { continue };
        if id == self_id || items.iter().any(|(i, _)| *i == id) {
            continue;
        }
        let name = c.get(2).map(|m| html_unescape(m.as_str().trim())).unwrap_or_default();
        items.push((id, name));
    }
    if items.is_empty() {
        RequiredItems::Unparsed
    } else {
        RequiredItems::Found(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sharedfiles_url() {
        assert_eq!(
            parse_workshop_id("https://steamcommunity.com/sharedfiles/filedetails/?id=104482086"),
            Some(104482086)
        );
    }

    #[test]
    fn parses_workshop_url_with_extra_params() {
        assert_eq!(
            parse_workshop_id("https://steamcommunity.com/workshop/filedetails/?l=english&id=128089118&searchtext=m9k"),
            Some(128089118)
        );
        assert_eq!(parse_workshop_id("steamcommunity.com/sharedfiles/filedetails/?id=123#comments"), Some(123));
        assert_eq!(
            parse_workshop_id("  https://STEAMCOMMUNITY.com/sharedfiles/filedetails/?searchtext=&id=777  "),
            Some(777)
        );
    }

    #[test]
    fn parses_plain_number() {
        assert_eq!(parse_workshop_id("104482086"), Some(104482086));
        assert_eq!(parse_workshop_id("  42 "), Some(42));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_workshop_id(""), None);
        assert_eq!(parse_workshop_id("abc"), None);
        assert_eq!(parse_workshop_id("0"), None);
        assert_eq!(parse_workshop_id("12a34"), None);
        assert_eq!(parse_workshop_id("https://example.com/?id=5"), None);
        assert_eq!(parse_workshop_id("https://steamcommunity.com/sharedfiles/filedetails/?uid=5"), None);
        assert_eq!(parse_workshop_id("https://steamcommunity.com/sharedfiles/filedetails/?id=abc"), None);
        assert_eq!(parse_workshop_id("https://steamcommunity.com/sharedfiles/filedetails/?id=12x"), None);
        assert_eq!(parse_workshop_id("99999999999999999999999999"), None);
    }

    #[test]
    fn required_items_block() {
        let html = r#"<div class="x"><div class="requiredItemsContainer" id="RequiredItems">
            <a href="https://steamcommunity.com/workshop/filedetails/?id=111" target="_blank">
                <div class="requiredItem">
                    Base &amp; Stuff
                </div>
            </a>
            <a href="https://steamcommunity.com/workshop/filedetails/?id=222" target="_blank">
                <div class="requiredItem">Other</div>
            </a>
        </div>
        <a href="https://steamcommunity.com/sharedfiles/filedetails/?id=333">not a dependency</a>"#;
        assert_eq!(
            parse_required_items(html, 999),
            RequiredItems::Found(vec![(111, "Base & Stuff".into()), (222, "Other".into())])
        );
        assert_eq!(parse_required_items("<html>nothing</html>", 1), RequiredItems::None);
        assert_eq!(parse_required_items(r#"<div class="requiredItemsContainer"></div>"#, 1), RequiredItems::Unparsed);
    }
}
