use async_trait::async_trait;
use log::{debug, warn};
use regex::Regex;
use reqwest::{header, Client, StatusCode};
use serde::Deserialize;
use std::cmp::Ordering;
use std::sync::OnceLock;
use tokio_util::sync::CancellationToken;

use super::{create_shared_client, CacheKeyScope, CoverArtProvider, CoverResult};
use crate::config::schema::{TmdbConfig, TmdbImageType, TmdbTvImageType};
use crate::cover::error::CoverArtError;
use crate::cover::sources::ArtSource;
use crate::metadata::MetadataSource;

const TMDB_API: &str = "https://api.themoviedb.org/3";
const TMDB_IMAGE_CDN: &str = "https://image.tmdb.org/t/p";

#[derive(Clone)]
pub struct TmdbProvider {
    client: Client,
    config: TmdbConfig,
    token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaKind {
    Movie {
        title: String,
        year: Option<u16>,
    },
    TvEpisode {
        show: String,
        season: u32,
        episode: u32,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct SearchResponse<T> {
    results: Vec<T>,
}

#[derive(Debug, Clone, Deserialize)]
struct MovieResult {
    id: u64,
    title: String,
    release_date: Option<String>,
    #[serde(default)]
    popularity: f64,
    #[serde(default)]
    vote_count: u32,
}
#[derive(Debug, Clone, Deserialize)]
struct TvResult {
    id: u64,
    name: String,
    first_air_date: Option<String>,
    #[serde(default)]
    popularity: f64,
    #[serde(default)]
    vote_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct ImagesResponse {
    #[serde(default)]
    posters: Vec<TmdbImage>,
    #[serde(default)]
    backdrops: Vec<TmdbImage>,
    #[serde(default)]
    stills: Vec<TmdbImage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TmdbImage {
    file_path: Option<String>,
    #[serde(default)]
    vote_average: f64,
    #[serde(default)]
    vote_count: u32,
    width: Option<u32>,
    height: Option<u32>,
}

impl TmdbProvider {
    pub fn with_config(config: TmdbConfig) -> Self {
        let token = std::env::var("TMDB_API_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty());
        if !config.enabled {
            debug!("TMDB provider disabled by configuration");
        } else if token.is_none() {
            warn!("TMDB token missing; set TMDB_API_TOKEN to enable TMDB cover art");
        } else {
            debug!("TMDB provider enabled");
        }
        Self {
            client: create_shared_client(),
            config,
            token,
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> Option<reqwest::RequestBuilder> {
        self.token.as_ref().map(|t| {
            req.bearer_auth(t)
                .header(header::ACCEPT, "application/json")
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, String)],
        cancel: &CancellationToken,
    ) -> Result<Option<T>, CoverArtError> {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let mut url = format!("{TMDB_API}{path}");
        if !params.is_empty() {
            let query = params
                .iter()
                .map(|(k, v)| format!("{}={}", k, urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&query);
        }
        let Some(req) = self.auth(self.client.get(url)) else {
            return Ok(None);
        };
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("TMDB request failed for {path}: {e}");
                return Ok(None);
            }
        };
        match resp.status() {
            s if s.is_success() => resp.json::<T>().await.map(Some).map_err(|e| {
                CoverArtError::provider_error("tmdb", &format!("malformed JSON: {e}"))
            }),
            StatusCode::UNAUTHORIZED => {
                warn!("TMDB request unauthorized; check TMDB_API_TOKEN");
                Ok(None)
            }
            StatusCode::TOO_MANY_REQUESTS => {
                warn!("TMDB rate limited request for {path}");
                Ok(None)
            }
            StatusCode::NOT_FOUND => {
                debug!("TMDB returned 404 for {path}");
                Ok(None)
            }
            s => {
                warn!("TMDB request for {path} returned HTTP {s}");
                Ok(None)
            }
        }
    }

    async fn search_movie(
        &self,
        title: &str,
        year: Option<u16>,
        cancel: &CancellationToken,
    ) -> Result<Option<MovieResult>, CoverArtError> {
        debug!("TMDB movie search query: {title}");
        let mut params = vec![("query", title.to_string())];
        if let Some(y) = year {
            params.push(("year", y.to_string()));
        }
        let Some(resp): Option<SearchResponse<MovieResult>> =
            self.get_json("/search/movie", &params, cancel).await?
        else {
            return Ok(None);
        };
        Ok(resp.results.into_iter().max_by(|a, b| {
            score_match(
                title,
                year,
                &a.title,
                a.release_date.as_deref(),
                a.popularity,
                a.vote_count,
            )
            .partial_cmp(&score_match(
                title,
                year,
                &b.title,
                b.release_date.as_deref(),
                b.popularity,
                b.vote_count,
            ))
            .unwrap_or(Ordering::Equal)
        }))
    }

    async fn search_tv(
        &self,
        show: &str,
        cancel: &CancellationToken,
    ) -> Result<Option<TvResult>, CoverArtError> {
        debug!("TMDB TV search query: {show}");
        let params = [("query", show.to_string())];
        let Some(resp): Option<SearchResponse<TvResult>> =
            self.get_json("/search/tv", &params, cancel).await?
        else {
            return Ok(None);
        };
        Ok(resp.results.into_iter().max_by(|a, b| {
            score_match(
                show,
                None,
                &a.name,
                a.first_air_date.as_deref(),
                a.popularity,
                a.vote_count,
            )
            .partial_cmp(&score_match(
                show,
                None,
                &b.name,
                b.first_air_date.as_deref(),
                b.popularity,
                b.vote_count,
            ))
            .unwrap_or(Ordering::Equal)
        }))
    }

    async fn image_url_for(
        &self,
        media: &MediaKind,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, CoverArtError> {
        match media {
            MediaKind::Movie { title, year } => {
                let Some(m) = self.search_movie(title, *year, cancel).await? else {
                    debug!("TMDB returned no movie match");
                    return Ok(None);
                };
                debug!("TMDB matched movie ID: {}", m.id);
                let Some(images): Option<ImagesResponse> = self
                    .get_json(&format!("/movie/{}/images", m.id), &[], cancel)
                    .await?
                else {
                    return Ok(None);
                };
                let list = match self.config.image_type {
                    TmdbImageType::Poster => images.posters,
                    TmdbImageType::Backdrop => images.backdrops,
                };
                self.select_url(list)
            }
            MediaKind::TvEpisode {
                show,
                season,
                episode,
            } => {
                let Some(tv) = self.search_tv(show, cancel).await? else {
                    debug!("TMDB returned no TV match");
                    return Ok(None);
                };
                debug!("TMDB matched TV series ID: {}", tv.id);
                for mode in fallback_modes(self.config.tv_image_type) {
                    let (path, kind) = match mode {
                        TmdbTvImageType::Episode => (
                            format!("/tv/{}/season/{}/episode/{}/images", tv.id, season, episode),
                            "episode still",
                        ),
                        TmdbTvImageType::Season => (
                            format!("/tv/{}/season/{}/images", tv.id, season),
                            "season poster",
                        ),
                        TmdbTvImageType::Series => {
                            (format!("/tv/{}/images", tv.id), "series poster")
                        }
                    };
                    let Some(images): Option<ImagesResponse> =
                        self.get_json(&path, &[], cancel).await?
                    else {
                        continue;
                    };
                    let list = match mode {
                        TmdbTvImageType::Episode => images.stills,
                        _ => images.posters,
                    };
                    if let Some(url) = self.select_url(list)? {
                        debug!("TMDB selected {kind}");
                        return Ok(Some(url));
                    }
                }
                Ok(None)
            }
        }
    }

    fn select_url(&self, images: Vec<TmdbImage>) -> Result<Option<String>, CoverArtError> {
        let ranked = rank_candidates(images);
        debug!("TMDB candidate artwork count: {}", ranked.len());
        let pool: Vec<_> = ranked.into_iter().take(8).collect();
        if pool.is_empty() {
            debug!("TMDB returned no usable artwork");
            return Ok(None);
        }
        let idx = random_index(pool.len());
        let img = &pool[idx];
        build_image_url(&self.config.image_size, img.file_path.as_deref().unwrap()).map(Some)
    }
}

#[async_trait]
impl CoverArtProvider for TmdbProvider {
    fn name(&self) -> &'static str {
        "tmdb"
    }
    fn cache_key_scope(&self) -> CacheKeyScope {
        CacheKeyScope::Metadata
    }
    fn supports_source_type(&self, _source: &ArtSource) -> bool {
        false
    }
    fn supports_metadata_only(&self) -> bool {
        self.config.enabled && self.token.is_some()
    }
    async fn process(
        &self,
        _source: ArtSource,
        metadata_source: &MetadataSource,
        cancel: &CancellationToken,
    ) -> Result<Option<CoverResult>, CoverArtError> {
        if !self.supports_metadata_only() {
            return Ok(None);
        }
        let Some(media) = detect_media_kind(metadata_source) else {
            debug!("TMDB artwork unavailable: no confident local movie/episode identity");
            return Ok(None);
        };
        debug!("TMDB detected media: {:?}", media);
        Ok(self
            .image_url_for(&media, cancel)
            .await?
            .map(|url| CoverResult {
                url,
                provider: "tmdb".to_string(),
                expiration: None,
            }))
    }
}

fn fallback_modes(first: TmdbTvImageType) -> Vec<TmdbTvImageType> {
    match first {
        TmdbTvImageType::Episode => vec![
            TmdbTvImageType::Episode,
            TmdbTvImageType::Season,
            TmdbTvImageType::Series,
        ],
        TmdbTvImageType::Season => vec![TmdbTvImageType::Season, TmdbTvImageType::Series],
        TmdbTvImageType::Series => vec![TmdbTvImageType::Series],
    }
}

pub fn detect_media_kind(metadata: &MetadataSource) -> Option<MediaKind> {
    let path = metadata.local_file_path();
    let title_fallback = metadata.title();
    let name = path
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .or(title_fallback.as_deref())?;
    parse_episode_filename(name)
        .map(|e| MediaKind::TvEpisode {
            show: e.show,
            season: e.season,
            episode: e.episode,
        })
        .or_else(|| clean_movie_title(name).map(|(title, year)| MediaKind::Movie { title, year }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeInfo {
    pub show: String,
    pub season: u32,
    pub episode: u32,
}

pub fn parse_episode_filename(name: &str) -> Option<EpisodeInfo> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)^(?P<show>.+?)[ ._\-]+(?:s(?P<s1>\d{1,2})e(?P<e1>\d{1,3})|(?P<s2>\d{1,2})x(?P<e2>\d{1,3}))\b").unwrap());
    let stem = strip_ext(name);
    let c = re.captures(stem)?;
    let show = clean_title_segment(c.name("show")?.as_str());
    let season = c
        .name("s1")
        .or_else(|| c.name("s2"))?
        .as_str()
        .parse()
        .ok()?;
    let episode = c
        .name("e1")
        .or_else(|| c.name("e2"))?
        .as_str()
        .parse()
        .ok()?;
    (!show.is_empty()).then_some(EpisodeInfo {
        show,
        season,
        episode,
    })
}

pub fn clean_movie_title(name: &str) -> Option<(String, Option<u16>)> {
    let stem = strip_ext(name);
    static YEAR: OnceLock<Regex> = OnceLock::new();
    let yr = YEAR.get_or_init(|| {
        Regex::new(r"(?i)(?:^|[ ._\-(])(?P<y>(?:19|20)\d{2})(?:$|[ ._\-)])").unwrap()
    });
    let year = yr
        .captures(stem)
        .and_then(|c| c.name("y"))
        .and_then(|m| m.as_str().parse().ok());
    let before = yr.find(stem).map(|m| &stem[..m.start()]).unwrap_or(stem);
    let title = clean_title_segment(before);
    (!title.is_empty()).then_some((title, year))
}

fn strip_ext(name: &str) -> &str {
    name.rsplit_once('.')
        .filter(|(_, e)| e.len() <= 5)
        .map(|(s, _)| s)
        .unwrap_or(name)
}

pub fn clean_title_segment(s: &str) -> String {
    let noise = [
        "1080p", "2160p", "720p", "480p", "web-dl", "webdl", "webrip", "bluray", "bdrip", "hdr",
        "dv", "x264", "x265", "h264", "h265", "h.264", "h.265", "hevc", "av1", "aac", "dts", "5.1",
        "7.1", "proper", "repack",
    ];
    s.replace(['.', '_'], " ")
        .replace('-', " ")
        .split_whitespace()
        .take_while(|p| !noise.contains(&p.to_ascii_lowercase().as_str()))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

pub fn rank_candidates(mut images: Vec<TmdbImage>) -> Vec<TmdbImage> {
    images.retain(|i| i.file_path.as_deref().is_some_and(|p| p.starts_with('/')));
    let good_count = images
        .iter()
        .filter(|i| i.vote_average >= 5.0 || i.vote_count == 0)
        .count();
    if good_count >= 3 {
        images.retain(|i| i.vote_average >= 5.0 || i.vote_count == 0);
    }
    images.sort_by(|a, b| {
        image_score(b)
            .partial_cmp(&image_score(a))
            .unwrap_or(Ordering::Equal)
    });
    images
}

fn image_score(i: &TmdbImage) -> f64 {
    let votes = (i.vote_count as f64 + 1.0).ln();
    let dim = match (i.width, i.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => ((w as f64 * h as f64).sqrt() / 1000.0).min(2.0),
        _ => 1.0,
    };
    i.vote_average * 10.0 + votes * 8.0 + dim
}

fn random_index(len: usize) -> usize {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return 0;
    }
    (u64::from_ne_bytes(bytes) as usize) % len
}

fn build_image_url(size: &str, file_path: &str) -> Result<String, CoverArtError> {
    if !file_path.starts_with('/') || size.contains('/') {
        return Err(CoverArtError::provider_error(
            "tmdb",
            "invalid image URL components",
        ));
    }
    Ok(format!("{TMDB_IMAGE_CDN}/{size}{file_path}"))
}

fn score_match(
    query: &str,
    year: Option<u16>,
    title: &str,
    date: Option<&str>,
    popularity: f64,
    vote_count: u32,
) -> f64 {
    let q = normalize(query);
    let t = normalize(title);
    let mut score = if q == t {
        100.0
    } else if t.contains(&q) || q.contains(&t) {
        82.0
    } else {
        token_overlap(&q, &t) * 70.0
    };
    if let (Some(y), Some(d)) = (year, date) {
        if d.starts_with(&y.to_string()) {
            score += 20.0;
        } else {
            score -= 8.0;
        }
    }
    score + popularity.ln_1p().min(8.0) + (vote_count as f64).ln_1p().min(6.0)
}
fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn token_overlap(a: &str, b: &str) -> f64 {
    let av: Vec<_> = a.split_whitespace().collect();
    if av.is_empty() {
        return 0.0;
    }
    let bv: std::collections::HashSet<_> = b.split_whitespace().collect();
    av.iter().filter(|x| bv.contains(**x)).count() as f64 / av.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_episode_filenames() {
        assert_eq!(
            parse_episode_filename("Show.Name.S02E04.1080p.mkv").unwrap(),
            EpisodeInfo {
                show: "Show Name".into(),
                season: 2,
                episode: 4
            }
        );
        assert_eq!(
            parse_episode_filename("Show Name - S01E01.mkv").unwrap(),
            EpisodeInfo {
                show: "Show Name".into(),
                season: 1,
                episode: 1
            }
        );
        assert_eq!(
            parse_episode_filename("Show.Name.2x03.mkv").unwrap(),
            EpisodeInfo {
                show: "Show Name".into(),
                season: 2,
                episode: 3
            }
        );
    }
    #[test]
    fn parses_movie_filenames() {
        assert_eq!(
            clean_movie_title("Movie.Name.1999.1080p.BluRay.mkv").unwrap(),
            ("Movie Name".into(), Some(1999))
        );
        assert_eq!(
            clean_movie_title("Movie Name (1999).mkv").unwrap(),
            ("Movie Name".into(), Some(1999))
        );
    }
    #[test]
    fn cleanup_preserves_legitimate_title() {
        assert_eq!(
            clean_title_segment("Star.Wars.Episode.I.The.Phantom.Menace.1080p.BluRay.x265"),
            "Star Wars Episode I The Phantom Menace"
        );
    }
    #[test]
    fn ranking_pushes_weak_candidates_down() {
        let weak = TmdbImage {
            file_path: Some("/weak.jpg".into()),
            vote_average: 1.0,
            vote_count: 1,
            width: Some(500),
            height: Some(750),
        };
        let strong = TmdbImage {
            file_path: Some("/strong.jpg".into()),
            vote_average: 8.0,
            vote_count: 50,
            width: Some(500),
            height: Some(750),
        };
        assert_eq!(
            rank_candidates(vec![weak, strong])[0].file_path.as_deref(),
            Some("/strong.jpg")
        );
    }
    #[tokio::test]
    async fn missing_token_disables_provider() {
        std::env::remove_var("TMDB_API_TOKEN");
        let p = TmdbProvider::with_config(TmdbConfig::default());
        assert!(!p.supports_metadata_only());
    }
    #[test]
    fn no_matching_media_without_metadata() {
        assert!(detect_media_kind(&MetadataSource::new(None, None)).is_none());
    }
    #[test]
    fn ambiguous_exact_beats_popular() {
        assert!(
            score_match("Crash", Some(2004), "Crash", Some("2004-05-06"), 1.0, 1)
                > score_match(
                    "Crash",
                    Some(2004),
                    "Crash Test",
                    Some("2020-01-01"),
                    1000.0,
                    9999
                )
        );
    }
}
