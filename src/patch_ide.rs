use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

use crate::patch_binary::RepatchOutcome;

/// Suffix of the pristine copy kept beside a JS file we rewrite. Same string the
/// bootstrap tooling uses, so they interoperate.
///
/// Why a backup at all (2.11.0_1): `patch_ide` REWRITES the auth function body,
/// which - unlike the same-length binary rename - cannot be reversed from the
/// patched bytes alone. Without a pristine copy, "Полный откат" left the IDE's
/// `main.js` patched forever (G25). The backup is written once, before the first
/// edit, and the revert restores from it.
const JS_BAK: &str = ".ag_backup";

fn backup_path(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(JS_BAK);
    PathBuf::from(s)
}

/// Copies `target` to its `.ag_backup` once - never overwriting an existing one,
/// which is by construction the pristine copy from before we ever touched it.
/// Best-effort: a failed backup must not stop a patch (the same-marker check
/// still prevents double-patching), but it is logged so a missing backup is not
/// silent.
fn backup_once(target: &Path) {
    let bak = backup_path(target);
    if bak.exists() {
        return;
    }
    if let Err(e) = fs::copy(target, &bak) {
        eprintln!("  [WARN] бэкап {} не создан: {}", target.display(), e);
    }
}

/// Writes `content` to `path` without a torn intermediate state: a sibling temp
/// on the same directory, then a rename over the target (atomic replace on
/// Windows and POSIX). A crash or power loss leaves either the old file or the
/// new one, never a half-written 15 MB `main.js`.
fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    // Per process, for the reason `patch_binary::temp_path` gives.
    crate::patch_binary::sweep_stale_temps(path);
    let tmp = crate::patch_binary::temp_path(path);
    if let Err(e) = fs::write(&tmp, content) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o644);
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(mode));
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Restores a JS file we rewrote from its pristine backup, and removes the
/// backup. Returns `Ok(true)` when it restored something, `Ok(false)` when there
/// was nothing of ours to undo.
///
/// A backup is the only faithful revert (the body was rewritten, not marked), so
/// this prefers it. If the backup is gone but our marker is present, it strips
/// the marker as a last resort and says so - the auth body then stays rewritten,
/// which still lets Antigravity run; a clean restore needs a reinstall.
fn restore_js(target: &Path) -> Result<bool, String> {
    let bak = backup_path(target);
    if bak.exists() {
        let data = fs::read(&bak).map_err(|e| format!("не прочитать бэкап: {}", e))?;
        let text = String::from_utf8_lossy(&data).into_owned();
        write_atomic(target, &text)
            .map_err(|e| format!("не записать {}: {}", target.display(), e))?;
        fs::remove_file(&bak).ok();
        return Ok(true);
    }
    let Ok(content) = fs::read_to_string(target) else {
        return Ok(false);
    };
    if let Some(stripped) = strip_marker(&content) {
        write_atomic(target, &stripped)
            .map_err(|e| format!("не записать {}: {}", target.display(), e))?;
        eprintln!(
            "  [WARN] {}: бэкап отсутствует — снят только маркер, тело патча осталось \
             (для чистого возврата переустановите Antigravity)",
            target.display()
        );
        return Ok(true);
    }
    Ok(false)
}

/// Reverts our IDE JS edits (`main.js` and the extension) for one install.
/// Returns how many files it restored.
/// Restores the IDE's JS from its pristine backups, one result per file.
///
/// Same reasoning as `patch_binary::unpatch_all_binaries`: a bare count turned a
/// failed restore into a silent success, and the messages went to a console the
/// window does not have.
pub fn unpatch_ide_js(inst: &Path) -> Vec<(String, Result<bool, String>)> {
    let mut out = Vec::new();
    let main_js = inst
        .join("resources")
        .join("app")
        .join("out")
        .join("main.js");
    if main_js.exists() {
        out.push(("main.js".to_string(), restore_js(&main_js)));
    }
    let ext = inst
        .join("resources")
        .join("app")
        .join("extensions")
        .join("antigravity")
        .join("dist")
        .join("extension.js");
    if ext.exists() {
        out.push(("extension.js".to_string(), restore_js(&ext)));
    }
    out
}

fn detect_stacked_ide(content: &str) -> bool {
    content.contains("/*[AG_PATCHED]*/") || content.contains("[AG_PROXY_HOOK]")
}

/// Every file we rewrite ends with this comment. Releases up to 2.5.0 wrote the
/// bare prefix; newer ones append the canaries (see `canary::file_marker`), so
/// detection matches on the prefix and stays compatible with installs patched by
/// an older build - `has_marker` decides "already patched", and getting that
/// wrong would either re-patch a patched file or refuse to revert one.
const MARKER_PREFIX: &str = "// UNLOCKED";

/// True when the last non-empty line is our marker, tokenised or legacy.
fn has_marker(content: &str) -> bool {
    let trimmed = content.trim_end();
    let last_line = trimmed.rsplit('\n').next().unwrap_or(trimmed);
    last_line.trim_start().starts_with(MARKER_PREFIX)
}

/// Drops a trailing marker line. Returns None when there is none, so callers can
/// tell "nothing to strip" from "stripped".
fn strip_marker(content: &str) -> Option<String> {
    if !has_marker(content) {
        return None;
    }
    let end = content.trim_end().len();
    match content[..end].rfind('\n') {
        Some(nl) => Some(content[..nl].to_string()),
        // Marker is the whole file: nothing but the marker to remove.
        None => Some(String::new()),
    }
}

pub fn patch_ide(_inst: &Path, main_js: &Path) -> Result<(), String> {
    let content = fs::read_to_string(main_js).map_err(|e| e.to_string())?;

    if has_marker(&content) && !detect_stacked_ide(&content) {
        return Ok(());
    }

    if detect_stacked_ide(&content) || has_marker(&content) {
        return Err("Обнаружена старая версия патча. Пожалуйста, выполните чистую переустановку Antigravity IDE перед повторным патчем.".into());
    }

    obfstr::obfstr! {
        let pattern_str = r#"async\s+([A-Za-z_$0-9]+)\(([A-Za-z_$0-9]+)\)\s*\{\s*if\(this\.([A-Za-z_$0-9]+)\.send\(\{type:[A-Za-z_$0-9]+\.isGcpTos\?"GCP_SIGN_IN":"SIGN_IN"\}\),this\.([A-Za-z_$0-9]+)\.resetIsTierGCPTos\(\),this\.[A-Za-z_$0-9]+\.isGoogleInternal\)\{try\{await this\.([A-Za-z_$0-9]+)\.loadCodeAssist\([A-Za-z_$0-9]+\);const\{settings:([A-Za-z_$0-9]+),userTier:([A-Za-z_$0-9]+)\}=await this\.refreshUserStatus\([A-Za-z_$0-9]+\),([A-Za-z_$0-9]+)=([A-Za-z_$0-9]+)\([A-Za-z_$0-9]+\);this\.([A-Za-z_$0-9]+)\.pushUpdate\([A-Za-z_$0-9]+\),this\.[A-Za-z_$0-9]+\.send\(\{type:"AUTH_SUCCESS",tokenInfo:[A-Za-z_$0-9]+\}\),this\.([A-Za-z_$0-9]+)\.fire\(\{settings:[A-Za-z_$0-9]+,userTier:[A-Za-z_$0-9]+\}\)\}catch\(([A-Za-z_$0-9]+)\)\{.*?(?:return\}|return;\s*\})"#;
    }

    let re = Regex::new(pattern_str).unwrap();
    if let Some(caps) = re.captures(&content) {
        let fname = caps.get(1).unwrap().as_str();
        let var_t = caps.get(2).unwrap().as_str();
        let var_t_send = caps.get(3).unwrap().as_str();
        let var_y = caps.get(4).unwrap().as_str();
        let var_func = caps.get(9).unwrap().as_str();
        let var_f = caps.get(10).unwrap().as_str();
        let var_h = caps.get(11).unwrap().as_str();
        let var_i = caps.get(8).unwrap().as_str();

        let payload = format!(
            r#"async {fname}({var_t}){{
    this.{var_t_send}.send({{type:{var_t}.isGcpTos?"GCP_SIGN_IN":"SIGN_IN"}});
    this.{var_y}.resetIsTierGCPTos();
    try {{
        try {{ await this.{var_y}.loadCodeAssist({var_t}); }} catch(_) {{}}
        try {{ await this.{var_y}.onboardUser("standard-tier", {var_t}); }} catch(_) {{
            try {{ await this.{var_y}.onboardUser("free-tier", {var_t}); }} catch(__) {{}}
        }}
        let __res = {{ settings: {{}}, userTier: {{ id: "pro", description: "Pro" }} }};
        try {{ __res = await this.refreshUserStatus({var_t}); }} catch(_) {{}}
        const {var_i} = {var_func}({var_t});
        try {{ this.{var_f}.pushUpdate({var_i}); }} catch(_) {{}}
        this.{var_t_send}.send({{type:"AUTH_SUCCESS",tokenInfo:{var_t}}});
        this.{var_h}.fire({{settings:__res.settings, userTier:__res.userTier}});
    }} catch(e) {{}}
    return;
"#
        );

        let new_content = format!(
            "{}\n{}",
            content[..caps.get(0).unwrap().start()].to_string()
                + &payload
                + &content[caps.get(0).unwrap().end()..],
            crate::canary::file_marker()
        );
        // Pristine copy before the first edit, so the revert can restore it (G25),
        // then an atomic write so a crash cannot leave a half-written main.js.
        backup_once(main_js);
        write_atomic(main_js, &new_content).map_err(|e| e.to_string())?;
        Ok(())
    } else {
        Err("Сигнатура не найдена (возможно, установлена другая версия)".to_string())
    }
}

/// Re-applies the IDE `main.js` patch after an update reverted it, classified
/// the same way as the binary rename so the watchdog can treat every target
/// uniformly.
///
/// `patch_ide` is fail-safe by construction - it writes only when its very
/// specific regex matches, and errors otherwise without touching the file - so
/// a build it does not understand yields `SignatureMissing` (leave it, show the
/// user the error) rather than a corrupted `main.js`. An I/O error (the file
/// briefly locked mid-update) stays `Failed` so it is retried, not written off.
pub fn repatch_ide_js(main_js: &Path) -> RepatchOutcome {
    let content = match fs::read_to_string(main_js) {
        Ok(c) => c,
        Err(e) => return RepatchOutcome::Failed(e.to_string()),
    };
    if has_marker(&content) && !detect_stacked_ide(&content) {
        return RepatchOutcome::AlreadyPatched;
    }
    match patch_ide(Path::new(""), main_js) {
        Ok(()) => RepatchOutcome::Repatched(1),
        // "Signature not found" and "old patch, reinstall" are both give-up
        // conditions - a human has to act, so surface the app's own error
        // instead of guessing. Anything else is an I/O problem worth retrying.
        Err(e) if e.contains("Сигнатура") || e.contains("старая версия") => {
            RepatchOutcome::SignatureMissing
        }
        Err(e) => RepatchOutcome::Failed(e),
    }
}

/// v2.4+ Desktop: modular Electron shell, auth lives in Language Server binary.
/// Verified against 2.4.2 and 2.5.0 - `dist/main.js` only wires up the shell
/// (`./languageServer`, `./ipcHandlers`) and carries no auth/eligibility code.
pub fn is_new_desktop_architecture(content: &str) -> bool {
    let modular = content.contains("./languageServer") && content.contains("./ipcHandlers");
    let legacy_auth = content.contains("_handleAuthErrorResponse")
        || content.contains("getUserStatus")
        || content.contains("\"AUTH_SUCCESS\"")
        || content.contains("\"SET_INELIGIBLE\"");
    modular && !legacy_auth
}

pub fn patch_desktop(_inst: &Path, main_js: &Path) -> Result<bool, String> {
    let content = fs::read_to_string(main_js).map_err(|e| e.to_string())?;

    let cleaned = strip_desktop_hook(&content);

    // v2.4+: auth handled entirely by Language Server binary (patched separately).
    // No JS modification needed — skip proxy hook and inline patches.
    if is_new_desktop_architecture(&cleaned) {
        if cleaned != content {
            fs::write(main_js, &cleaned).map_err(|e| e.to_string())?;
            println!("  [INFO] Удалён устаревший JS-патч");
        }
        println!("  [INFO] v2.4+ архитектура — JS-патч не требуется (auth в Language Server)");
        return Ok(false);
    }

    let inline_patched = apply_desktop_inline_patches(&cleaned);

    obfstr::obfstr! {
        let hook = r#"// [AG_PROXY_HOOK]
const _elec = require('electron');
const http = require('http');
const https = require('https');
const zlib = require('zlib');

let agProxyPort = 0;
let realLsPort = 0;
let agApiPort = 0;

const proxyServer = http.createServer((req, res) => {
    if (!realLsPort) {
        if (!res.headersSent) res.writeHead(500);
        return res.end('No LS port');
    }
    const options = {
        hostname: '127.0.0.1',
        port: realLsPort,
        path: req.url,
        method: req.method,
        headers: req.headers,
        rejectUnauthorized: false
    };

    const proxyReq = https.request(options, (proxyRes) => {
        if (req.url.includes('main.js')) {
            const chunks = [];
            proxyRes.on('data', c => chunks.push(c));
            proxyRes.on('end', () => {
                let buffer = Buffer.concat(chunks);
                const encoding = (proxyRes.headers['content-encoding'] || '').toLowerCase();
                let wasCompressed = false;
                if (encoding.includes('gzip')) {
                    try { buffer = zlib.gunzipSync(buffer); wasCompressed = true; } catch(e){}
                } else if (encoding.includes('br')) {
                    try { buffer = zlib.brotliDecompressSync(buffer); wasCompressed = true; } catch(e){}
                }

                let body = buffer.toString('utf-8');

                body = body.replace(/_handleAuthErrorResponse\(([a-zA-Z_$]+)\)\{var ([a-zA-Z_$]+)=\1\?\.failureDetails;/g, '_handleAuthErrorResponse($1){var $2=$1?.failureDetails; if($2?.case==="ineligible"){ this._authActor.send({type:"AUTH_SUCCESS",tokenInfo:{accessToken:""},scopes:[],isGcpTos:false}); return; }');
                body = body.replace(/\?\.failureDetails\?\.case==="ineligible"\?this\._authActor\.send\(\{type:"SET_INELIGIBLE"/g, '?.failureDetails?.case==="NEVER_MATCH"?this._authActor.send({type:"SET_INELIGIBLE"');

                const pattern2 = /let ([A-Za-z_$]+)=.*\.getUserStatus\(\{\}\)\)\)\.userStatus;if\(\1\)\{/g;
                body = body.replace(pattern2, (match, p1) => {
                    return match.replace(`if(${p1}){`, `${p1}={planStatus:{planInfo:{planName:"pro"}}, disableTelemetry:false, userDataCollectionForceDisabled:false};if(${p1}){`);
                });

                let outBuffer = Buffer.from(body, 'utf-8');
                if (wasCompressed) {
                    if (encoding.includes('gzip')) outBuffer = zlib.gzipSync(outBuffer);
                    else if (encoding.includes('br')) outBuffer = zlib.brotliCompressSync(outBuffer);
                }

                const headers = { ...proxyRes.headers };
                headers['content-length'] = outBuffer.length;
                if (!res.headersSent) res.writeHead(proxyRes.statusCode, headers);
                res.end(outBuffer);
            });
        } else {
            if (!res.headersSent) res.writeHead(proxyRes.statusCode, proxyRes.headers);
            proxyRes.pipe(res, { end: true });
        }
    });

    proxyReq.on('error', (e) => {
        if (!res.headersSent) res.writeHead(500);
        res.end();
    });
    req.pipe(proxyReq, { end: true });
});

proxyServer.listen(0, '127.0.0.1', () => { agProxyPort = proxyServer.address().port; });

const apiProxyServer = http.createServer((req, res) => {
    res.setHeader('Content-Type', 'application/json');
    res.setHeader('Access-Control-Allow-Origin', '*');
    const body = [];
    req.on('data', c => body.push(c));
    req.on('end', () => {
        const reqBody = Buffer.concat(body).toString('utf-8');
        if (req.url.includes('UserStatus') || req.url.includes('getUserStatus') || req.url.includes('refreshUserStatus') || req.url.includes('userStatus')) {
            return res.end(JSON.stringify({
                userStatus: {
                    userTier: { id: "pro", description: "Pro" },
                    currentTier: { id: "STANDARD", hasOnboardedPreviously: true },
                    planStatus: { planInfo: { planName: "pro", isEligible: true } },
                    eligible: true,
                    ineligibleReason: null,
                    disableTelemetry: false,
                    userDataCollectionForceDisabled: false,
                    allowedTiers: [{ id: "STANDARD", name: "Standard", isDefault: true }]
                }
            }));
        }
        if (req.url.includes('loadCodeAssist')) {
            return res.end(JSON.stringify({
                currentTier: { id: "STANDARD", hasOnboardedPreviously: true },
                cloudaicompanionProject: "",
                allowedTiers: [{ id: "STANDARD", name: "Standard", isDefault: true }],
                paidTier: void 0
            }));
        }
        if (req.url.includes('onboardUser')) {
            return res.end(JSON.stringify({
                done: true,
                response: {
                    cloudaicompanionProject: { id: "" }
                }
            }));
        }
        if (req.url.includes('listExperiments')) {
            return res.end(JSON.stringify({ flags: [], experimentIds: [] }));
        }
        if (req.url.includes('fetchAdminControls')) {
            return res.end(JSON.stringify({}));
        }
        if (req.url.includes('getCodeAssistGlobalUserSetting')) {
            return res.end(JSON.stringify({}));
        }
        if (req.url.includes('refreshUserQuota') || req.url.includes('retrieveUserQuota')) {
            return res.end(JSON.stringify({}));
        }
        if (req.url.includes('listAvailableTiers') || req.url.includes('isEligible')) {
            return res.end(JSON.stringify({ tiers: [], isEligible: true, ineligibleReason: null }));
        }
        return res.end(JSON.stringify({
            eligible: true,
            ineligibleReason: null,
            allowedTiers: [{ id: "STANDARD", name: "Standard", isDefault: true }],
            currentTier: { id: "STANDARD", hasOnboardedPreviously: true },
            planStatus: { planInfo: { planName: "pro", isEligible: true } },
            userTier: { id: "pro", description: "Pro" }
        }));
    });
});

apiProxyServer.listen(0, '127.0.0.1', () => { agApiPort = apiProxyServer.address().port; });

const _origWhenReady = _elec.app.whenReady;
_elec.app.whenReady = function() {
    return _origWhenReady.call(this).then(() => {
        _elec.session.defaultSession.webRequest.onBeforeRequest({ urls: ['*://*.googleapis.com/*', '*://127.0.0.1:*/*', '*://localhost:*/*'] }, (details, callback) => {
            const urlObj = new URL(details.url);
            if (urlObj.hostname.includes('googleapis.com')) {
                return callback({ redirectURL: `http://127.0.0.1:${agApiPort}${urlObj.pathname}` });
            }
            if (urlObj.port != agProxyPort && !details.url.includes('ag_bypass')) {
                realLsPort = urlObj.port;
                if (details.url.includes('.js') || details.url.includes('.css') || details.url.includes('.png') || details.url.includes('.woff') || details.url.includes('main.js') || details.url.includes('index.html') || details.url.includes('/')) {
                    urlObj.protocol = 'http:'; urlObj.port = agProxyPort; urlObj.searchParams.set('ag_bypass', '1');
                    return callback({ redirectURL: urlObj.toString() });
                }
            }
            callback({});
        });
    });
};
// [/AG_PROXY_HOOK]
"#;
    }

    let new_content = format!(
        "{}\n{}\n{}",
        hook,
        inline_patched,
        crate::canary::file_marker()
    );
    fs::write(main_js, new_content).map_err(|e| e.to_string())?;
    Ok(true)
}

fn strip_desktop_hook(content: &str) -> String {
    if let Some(start) = content.find("// [AG_PROXY_HOOK]") {
        if let Some(end) = content[start..].find("// [/AG_PROXY_HOOK]") {
            let after = start + end + "// [/AG_PROXY_HOOK]".len();
            let mut rest = content[after..].to_string();
            if let Some(stripped) = strip_marker(&rest) {
                rest = stripped;
            }
            return content[..start].to_string() + &rest;
        }
    }
    if let Some(stripped) = strip_marker(content) {
        return stripped + "\n";
    }
    content.to_string()
}

fn apply_desktop_inline_patches(content: &str) -> String {
    let mut output = content.to_string();

    output = output.replace("ineligible", "inexigible");

    let re_getus = Regex::new(
        r#"let ([A-Za-z_$]+)=.*\.getUserStatus\(\{\}\)\)\)\.userStatus;if\([A-Za-z_$]+\)\{"#,
    )
    .unwrap();
    output = re_getus.replace_all(&output, |caps: &regex::Captures| {
        if caps.get(1).map_or("", |m| m.as_str()) != caps.get(2).map_or("", |m| m.as_str()) {
            return caps.get(0).map_or("", |m| m.as_str()).to_string();
        }
        let v = caps.get(1).map_or("", |m| m.as_str());
        format!("let {v}=...getUserStatus({{}}))).userStatus;{v}={{\"planStatus\":{{\"planInfo\":{{\"planName\":\"pro\"}}}},\"disableTelemetry\":false,\"userDataCollectionForceDisabled\":false}};if({v}){{")
    }).to_string();

    let re_auth = Regex::new(r#"_handleAuthErrorResponse\(([A-Za-z_$]+)\)\{var ([A-Za-z_$]+)=([A-Za-z_$]+)\?\.failureDetails;"#).unwrap();
    output = re_auth.replace_all(&output, |caps: &regex::Captures| {
        let p = caps.get(1).map_or("", |m| m.as_str());
        let v = caps.get(2).map_or("", |m| m.as_str());
        let s = caps.get(3).map_or("", |m| m.as_str());
        if p != s {
            return caps.get(0).map_or("", |m| m.as_str()).to_string();
        }
        format!("_handleAuthErrorResponse({p}){{var {v}={p}?.failureDetails; if({v}?.case===\"inexigible\"){{ this._authActor.send({{type:\"AUTH_SUCCESS\",tokenInfo:{{accessToken:\"\"}},scopes:[],isGcpTos:false}}); return; }}")
    }).to_string();

    let re_inel = Regex::new(r#"\?\.failureDetails\?\.case===\"ineligible\"\?this\._authActor\.send\(\{type:\"SET_INELIGIBLE\""#).unwrap();
    output = re_inel
        .replace_all(&output, |_caps: &regex::Captures| {
            "?.failureDetails?.case===\"NEVER_MATCH\"?this._authActor.send({type:\"SET_INELIGIBLE\""
        })
        .to_string();

    let re_eligible = Regex::new(r#"(isEligible:\s*)false"#).unwrap();
    output = re_eligible
        .replace_all(&output, |caps: &regex::Captures| {
            format!("{}true", &caps[1])
        })
        .to_string();

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn detects_a_marker_written_by_this_build() {
        let js = format!("code();\n{}", crate::canary::file_marker());
        assert!(has_marker(&js));
    }

    #[test]
    fn detects_the_legacy_bare_marker() {
        // Installs patched by <=2.5.0 must still read as "already patched",
        // otherwise an upgrade re-patches an already patched file.
        assert!(has_marker("code();\n// UNLOCKED"));
        assert!(has_marker("code();\n// UNLOCKED\n\n"));
    }

    #[test]
    fn an_unpatched_file_has_no_marker() {
        assert!(!has_marker("code();\n// some other trailing comment"));
        assert!(!has_marker("// UNLOCKED is mentioned mid-file\ncode();"));
        assert!(!has_marker(""));
    }

    /// The G25 fix, end to end on real files: a backup taken before a rewrite is
    /// what `restore_js` uses to put the exact original bytes back, and the backup
    /// is then removed. Without this the full revert left main.js patched.
    #[test]
    fn a_backup_restores_the_exact_original_and_is_removed() {
        let dir = std::env::temp_dir().join("ag_js_restore_test");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("main.js");
        let original = "// pristine main.js\nconst x = 1;\n";
        fs::write(&target, original).unwrap();

        // First backup captures the pristine file.
        backup_once(&target);
        assert!(backup_path(&target).exists());
        // A "patch": rewrite the body and append a marker.
        write_atomic(&target, "REWRITTEN BODY\n// UNLOCKED").unwrap();
        // A second backup must NOT overwrite the pristine one.
        backup_once(&target);

        assert_eq!(restore_js(&target).unwrap(), true);
        assert_eq!(fs::read_to_string(&target).unwrap(), original, "byte-exact");
        assert!(!backup_path(&target).exists(), "backup consumed");
        // Nothing left to undo now.
        assert_eq!(restore_js(&target).unwrap(), false);

        fs::remove_dir_all(&dir).ok();
    }

    /// With no backup but our marker present, restore strips the marker as a last
    /// resort (the body stays rewritten) rather than doing nothing.
    #[test]
    fn restore_without_a_backup_at_least_strips_the_marker() {
        let dir = std::env::temp_dir().join("ag_js_restore_nombak");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("main.js");
        fs::write(&target, "body\n// UNLOCKED").unwrap();

        assert_eq!(restore_js(&target).unwrap(), true);
        assert!(!has_marker(&fs::read_to_string(&target).unwrap()));

        fs::remove_dir_all(&dir).ok();
    }

    /// A crash mid-write must never leave a torn target: `write_atomic` replaces
    /// via a rename, so the target is only ever the old or the new content, and
    /// the temp is cleaned up.
    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = std::env::temp_dir().join("ag_js_atomic");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("f.js");
        fs::write(&target, "old").unwrap();
        write_atomic(&target, "new content").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new content");
        let tmp = crate::patch_binary::temp_path(&target);
        assert!(!tmp.exists(), "temp cleaned up");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn strip_marker_removes_both_shapes_and_nothing_else() {
        let body = "line one\nline two";
        for marker in [crate::canary::file_marker(), "// UNLOCKED".to_string()] {
            let patched = format!("{}\n{}", body, marker);
            assert_eq!(strip_marker(&patched).as_deref(), Some(body));
        }
        assert!(strip_marker(body).is_none());
    }

    #[test]
    fn recognises_the_shipping_v24_plus_shell() {
        let Ok(local) = env::var("LOCALAPPDATA") else {
            return;
        };
        let asar = Path::new(&local)
            .join("Programs")
            .join("Antigravity")
            .join("resources")
            .join("app.asar");
        if !asar.exists() {
            return;
        }
        let src = crate::asar::read_asar_entry(&asar, "dist/main.js")
            .and_then(|b| String::from_utf8(b).ok())
            .expect("dist/main.js");
        assert!(
            is_new_desktop_architecture(&src),
            "installed Desktop build was classified as legacy - the JS patch would be applied"
        );
    }

    #[test]
    fn a_legacy_shell_still_needs_the_js_patch() {
        let legacy = r#"require("./languageServer");require("./ipcHandlers");
            _handleAuthErrorResponse(e){var t=e?.failureDetails;}"#;
        assert!(!is_new_desktop_architecture(legacy));
    }

    #[test]
    fn a_non_modular_shell_is_not_v24() {
        assert!(!is_new_desktop_architecture("const x = 1;"));
    }
}

pub fn patch_extension_js(inst: &Path) -> Result<bool, String> {
    let ext_path = inst
        .join("resources")
        .join("app")
        .join("extensions")
        .join("antigravity")
        .join("dist")
        .join("extension.js");
    if !ext_path.exists() {
        return Ok(false);
    }

    let content = fs::read_to_string(&ext_path).map_err(|e| e.to_string())?;
    if content.contains("/*[AG_EXT_PATCHED]*/") {
        return Ok(true);
    }

    obfstr::obfstr! {
        let p1_str = r#"const t=await ([A-Za-z_$][A-Za-z_$0-9.]*)\.UserStatus\.getUserStatus\(\);if\(!t\)return\[\];const n=\(0,([A-Za-z_$][A-Za-z_$0-9.]*)\)\(t,([A-Za-z_$][A-Za-z_$0-9.]*)\),\{email:([A-Za-z_$][A-Za-z_$0-9]*),name:([A-Za-z_$][A-Za-z_$0-9]*)\}=n;return""===([A-Za-z_$][A-Za-z_$0-9]*)\?\[\]:"#;
    }
    let p1 = Regex::new(p1_str).unwrap();
    let new_content = p1.replace(&content, |caps: &regex::Captures| {
        let ns = &caps[1];
        let p2 = &caps[2];
        let dz7 = &caps[3];
        let email = &caps[4];
        let name = &caps[5];
        format!(
            "const t=await {ns}.UserStatus.getUserStatus();let {email}=\"\",{name}=\"\";try{{if(t){{const n=(0,{p2})(t,{dz7});{email}=n.email||\"\";{name}=n.name||\"\";}}}}catch(_){{}}if({email}===\"\"){{{email}=\"antigravity-user\";{name}=\"User\";}}return false?[]:"
        )
    });

    if new_content == content {
        return Err("Сигнатура extension.js не найдена (возможно, другая версия)".to_string());
    }

    // Leading fence stays the idempotence check; the trailing marker is the
    // canary trailer every rewritten file carries.
    let marked = format!(
        "/*[AG_EXT_PATCHED]*/\n{}\n{}",
        new_content,
        crate::canary::file_marker()
    );
    backup_once(&ext_path);
    write_atomic(&ext_path, &marked).map_err(|e| e.to_string())?;
    Ok(true)
}

const REMOTE_SH_BEGIN: &str = "# >>> ag-unlocker >>>";
const REMOTE_SH_END: &str = "# <<< ag-unlocker <<<";
const REMOTE_SH_HOOK: &str = "\
# >>> ag-unlocker >>>
_ag_c=\"${XDG_CONFIG_HOME:-$HOME/.config}/environment.d/ag-unlocker.conf\"
if [ -r \"$_ag_c\" ]; then set -a; . \"$_ag_c\"; set +a; fi
# <<< ag-unlocker <<<";

const REMOTE_JS_BEGIN: &str = "/* [AG_ENV_HOOK] */";
const REMOTE_JS_END: &str = "/* [/AG_ENV_HOOK] */";
const REMOTE_JS_CJS_HOOK: &str = "\
/* [AG_ENV_HOOK] */
(()=>{try{const _f=require(\"fs\"),_o=require(\"os\"),_p=require(\"path\"),_b=process.env.XDG_CONFIG_HOME||_p.join(_o.homedir(),\".config\"),_c=_p.join(_b,\"environment.d\",\"ag-unlocker.conf\");if(_f.existsSync(_c)){for(const _l of _f.readFileSync(_c,\"utf8\").split(/\\r?\\n/)){const _t=_l.trim();if(!_t||_t.startsWith(\"#\"))continue;const _i=_t.indexOf(\"=\");if(_i>0&&_t.slice(0,_i).trim()===\"AG_LS_PROXY\"){const _v=_t.slice(_i+1).trim().replace(/^['\"]|['\"]$/g,\"\");if(_v)process.env.AG_LS_PROXY=_v;}}}}catch(_){}})();
/* [/AG_ENV_HOOK] */";

const REMOTE_JS_ESM_HOOK: &str = "\
/* [AG_ENV_HOOK] */
import * as __agFs from \"node:fs\";
import * as __agOs from \"node:os\";
import * as __agPath from \"node:path\";
try{const _b=process.env.XDG_CONFIG_HOME||__agPath.join(__agOs.homedir(),\".config\"),_c=__agPath.join(_b,\"environment.d\",\"ag-unlocker.conf\");if(__agFs.existsSync(_c)){for(const _l of __agFs.readFileSync(_c,\"utf8\").split(/\\r?\\n/)){const _t=_l.trim();if(!_t||_t.startsWith(\"#\"))continue;const _i=_t.indexOf(\"=\");if(_i>0&&_t.slice(0,_i).trim()===\"AG_LS_PROXY\"){const _v=_t.slice(_i+1).trim().replace(/^['\"]|['\"]$/g,\"\");if(_v)process.env.AG_LS_PROXY=_v;}}}}catch(_){}
/* [/AG_ENV_HOOK] */";

fn is_posix_shell_script(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    let mut buf = [0u8; 256];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    let head = &buf[..n];
    if !head.starts_with(b"#!") {
        return false;
    }
    let first_line_end = head.iter().position(|&b| b == b'\n').unwrap_or(head.len());
    let first_line = String::from_utf8_lossy(&head[..first_line_end]).to_ascii_lowercase();
    first_line.contains("sh")
        && !first_line.contains("node")
        && !first_line.contains("python")
        && !first_line.contains("perl")
        && !first_line.contains("ruby")
}

fn first_js_statement(mut s: &str) -> &str {
    if s.starts_with("#!") {
        match s.find('\n') {
            Some(nl) => s = &s[nl + 1..],
            None => return "",
        }
    }
    loop {
        s = s.trim_start();
        if s.starts_with("//") {
            match s.find('\n') {
                Some(nl) => s = &s[nl + 1..],
                None => return "",
            }
        } else if s.starts_with("/*") {
            match s.find("*/") {
                Some(end) => s = &s[end + 2..],
                None => return "",
            }
        } else {
            return s;
        }
    }
}

fn is_esm_server_main(path: &Path, content: &str) -> bool {
    if let Some(pkg) = path
        .parent()
        .and_then(|p| p.parent())
        .map(|r| r.join("package.json"))
    {
        if let Ok(text) = fs::read_to_string(pkg) {
            if let Ok(re) = Regex::new(r#""type"\s*:\s*"module""#) {
                if re.is_match(&text) {
                    return true;
                }
            }
        }
    }
    let stmt = first_js_statement(content);
    stmt.starts_with("import ")
        || stmt.starts_with("import{")
        || stmt.starts_with("import*")
        || stmt.starts_with("import\"")
        || stmt.starts_with("import'")
}

fn insert_after_shebang_and_strict(content: &str, hook: &str, keep_strict: bool) -> String {
    let mut pos = 0usize;
    if content.starts_with("#!") {
        pos = content.find('\n').map_or(content.len(), |nl| nl + 1);
    }
    if keep_strict && pos < content.len() {
        let rest = &content[pos..];
        let trimmed = rest.trim_start_matches([' ', '\t', '\r', '\n']);
        let lead = rest.len() - trimmed.len();
        for lit in ["\"use strict\";", "'use strict';"] {
            if let Some(after) = trimmed.strip_prefix(lit) {
                let eol = if after.starts_with("\r\n") {
                    2
                } else if after.starts_with('\n') {
                    1
                } else {
                    0
                };
                pos += lead + lit.len() + eol;
                break;
            }
        }
        if pos == content.len() - rest.len() {
            for bare in ["\"use strict\"\r\n", "'use strict'\r\n", "\"use strict\"\n", "'use strict'\n"] {
                if trimmed.starts_with(bare) {
                    pos += lead + bare.len();
                    break;
                }
            }
        }
    }
    let prefix = &content[..pos];
    let suffix = &content[pos..];
    if !prefix.is_empty() && !prefix.ends_with('\n') {
        if pos == content.len() {
            return format!("{prefix}\n{hook}\n");
        }
        return format!("{prefix}{hook}\n{suffix}");
    }
    format!("{prefix}{hook}\n{suffix}")
}

fn strip_fenced_block(content: &str, begin: &str, end: &str) -> Option<String> {
    let b = content.find(begin)?;
    let e_rel = content[b..].find(end)?;
    let mut after = b + e_rel + end.len();
    if content[after..].starts_with("\r\n") {
        after += 2;
    } else if content[after..].starts_with('\n') {
        after += 1;
    }
    Some(format!("{}{}", &content[..b], &content[after..]))
}

pub fn remote_env_targets(inst: &Path) -> Vec<PathBuf> {
    if !crate::is_remote_server_install(inst) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let push_unique = |list: &mut Vec<PathBuf>, p: PathBuf| {
        if !list.contains(&p) {
            list.push(p);
        }
    };
    for srv in crate::patch_binary::remote_server_dirs(inst) {
        for cand in [
            srv.join("bin").join("antigravity-server"),
            srv.join("bin").join("antigravity-server-insiders"),
            srv.join("bin").join("code-server"),
            srv.join("server.sh"),
        ] {
            if cand.is_file() && is_posix_shell_script(&cand) {
                push_unique(&mut out, cand);
            }
        }
        let server_main = srv.join("out").join("server-main.js");
        if server_main.is_file() {
            push_unique(&mut out, server_main);
        }
    }
    out
}

pub fn is_remote_env_target(path: &Path) -> bool {
    let is_server_main = path.file_name().is_some_and(|n| n == "server-main.js")
        && path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == "out");
    if is_server_main {
        return true;
    }
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        matches!(
            n,
            "antigravity-server" | "antigravity-server-insiders" | "code-server" | "server.sh"
        )
    })
}

pub fn repatch_remote_env_file(target: &Path) -> RepatchOutcome {
    let is_js = target.extension().and_then(|e| e.to_str()) == Some("js");
    if !is_js && !is_posix_shell_script(target) {
        return RepatchOutcome::AlreadyPatched;
    }
    let content = match fs::read_to_string(target) {
        Ok(c) => c,
        Err(e) => return RepatchOutcome::Failed(e.to_string()),
    };
    if is_js {
        if content.contains(REMOTE_JS_BEGIN) {
            return RepatchOutcome::AlreadyPatched;
        }
        let esm = is_esm_server_main(target, &content);
        let hook = if esm {
            REMOTE_JS_ESM_HOOK
        } else {
            REMOTE_JS_CJS_HOOK
        };
        let updated = insert_after_shebang_and_strict(&content, hook, !esm);
        return match write_atomic(target, &updated) {
            Ok(()) => RepatchOutcome::Repatched(1),
            Err(e) => RepatchOutcome::Failed(e.to_string()),
        };
    }
    if content.contains(REMOTE_SH_BEGIN) {
        return RepatchOutcome::AlreadyPatched;
    }
    let updated = insert_after_shebang_and_strict(&content, REMOTE_SH_HOOK, false);
    match write_atomic(target, &updated) {
        Ok(()) => RepatchOutcome::Repatched(1),
        Err(e) => RepatchOutcome::Failed(e.to_string()),
    }
}

pub fn patch_remote_server_env(inst: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    for target in remote_env_targets(inst) {
        if let RepatchOutcome::Failed(e) = repatch_remote_env_file(&target) {
            let name = target.file_name().unwrap_or_default().to_string_lossy();
            warnings.push(format!("{}: {}", name, e));
        }
    }
    warnings
}

pub fn unpatch_remote_server_env(inst: &Path) -> Vec<(String, Result<bool, String>)> {
    let mut out = Vec::new();
    for target in remote_env_targets(inst) {
        let label = target
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let is_js = target.extension().and_then(|e| e.to_str()) == Some("js");
        let content = match fs::read_to_string(&target) {
            Ok(c) => c,
            Err(e) => {
                out.push((label, Err(e.to_string())));
                continue;
            }
        };
        let (begin, end) = if is_js {
            (REMOTE_JS_BEGIN, REMOTE_JS_END)
        } else {
            (REMOTE_SH_BEGIN, REMOTE_SH_END)
        };
        match strip_fenced_block(&content, begin, end) {
            Some(stripped) => {
                let res = write_atomic(&target, &stripped)
                    .map(|()| true)
                    .map_err(|e| format!("не записать {}: {}", target.display(), e));
                out.push((label, res));
            }
            None => out.push((label, Ok(false))),
        }
    }
    out
}

#[cfg(test)]
mod remote_env_tests {
    use super::*;

    #[test]
    fn shell_launcher_patches_idempotently_and_reverts_byte_exactly() {
        let root = std::env::temp_dir().join(format!("ag_remote_sh_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let commit = root.join(".antigravity-server").join("bin").join("c1");
        let bin = commit.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let sh = bin.join("antigravity-server");
        let orig = "#!/usr/bin/env sh\nset -e\nexec node out/server-main.js \"$@\"\n";
        fs::write(&sh, orig).unwrap();

        let targets = remote_env_targets(&root.join(".antigravity-server"));
        assert_eq!(targets, vec![sh.clone()]);
        assert!(is_remote_env_target(&sh));

        assert_eq!(repatch_remote_env_file(&sh), RepatchOutcome::Repatched(1));
        let patched = fs::read_to_string(&sh).unwrap();
        assert!(patched.starts_with("#!/usr/bin/env sh\n# >>> ag-unlocker >>>\n"));
        assert!(patched.contains("exec node out/server-main.js"));
        assert_eq!(repatch_remote_env_file(&sh), RepatchOutcome::AlreadyPatched);

        let rev = unpatch_remote_server_env(&root.join(".antigravity-server"));
        assert_eq!(rev.len(), 1);
        assert_eq!(rev[0].1, Ok(true));
        assert_eq!(fs::read_to_string(&sh).unwrap(), orig);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn elf_binary_named_antigravity_server_is_skipped_and_untouched() {
        let root = std::env::temp_dir().join(format!("ag_remote_elf_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let bin = root
            .join(".antigravity-server")
            .join("bin")
            .join("c1")
            .join("bin");
        fs::create_dir_all(&bin).unwrap();
        let elf = bin.join("antigravity-server");
        let orig = b"\x7fELF\x02\x01\x01\x00binary";
        fs::write(&elf, orig).unwrap();

        assert!(remote_env_targets(&root.join(".antigravity-server")).is_empty());
        assert_eq!(
            repatch_remote_env_file(&elf),
            RepatchOutcome::AlreadyPatched
        );
        assert_eq!(fs::read(&elf).unwrap(), orig);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cjs_server_main_patches_after_use_strict_and_reverts_byte_exactly() {
        let root = std::env::temp_dir().join(format!("ag_remote_cjs_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let srv = root.join(".antigravity-server").join("bin").join("c1");
        let out_dir = srv.join("out");
        fs::create_dir_all(&out_dir).unwrap();
        let js = out_dir.join("server-main.js");
        let orig = "\"use strict\";\nconst net = require(\"net\");\n";
        fs::write(&js, orig).unwrap();

        assert!(is_remote_env_target(&js));
        assert_eq!(repatch_remote_env_file(&js), RepatchOutcome::Repatched(1));
        let patched = fs::read_to_string(&js).unwrap();
        assert!(patched.starts_with("\"use strict\";\n/* [AG_ENV_HOOK] */\n"));
        assert!(patched.contains("require(\"fs\")"));
        assert_eq!(repatch_remote_env_file(&js), RepatchOutcome::AlreadyPatched);

        let rev = unpatch_remote_server_env(&root.join(".antigravity-server"));
        assert_eq!(rev, vec![("server-main.js".to_string(), Ok(true))]);
        assert_eq!(fs::read_to_string(&js).unwrap(), orig);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn esm_server_main_uses_import_hook_and_reverts_byte_exactly() {
        let root = std::env::temp_dir().join(format!("ag_remote_esm_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let srv = root.join(".antigravity-server").join("bin").join("c1");
        let out_dir = srv.join("out");
        fs::create_dir_all(&out_dir).unwrap();
        fs::write(srv.join("package.json"), r#"{"type": "module"}"#).unwrap();
        let js = out_dir.join("server-main.js");
        let orig = "import http from \"node:http\";\n";
        fs::write(&js, orig).unwrap();

        assert_eq!(repatch_remote_env_file(&js), RepatchOutcome::Repatched(1));
        let patched = fs::read_to_string(&js).unwrap();
        assert!(patched.starts_with("/* [AG_ENV_HOOK] */\nimport * as __agFs"));
        assert_eq!(repatch_remote_env_file(&js), RepatchOutcome::AlreadyPatched);

        let rev = unpatch_remote_server_env(&root.join(".antigravity-server"));
        assert_eq!(rev, vec![("server-main.js".to_string(), Ok(true))]);
        assert_eq!(fs::read_to_string(&js).unwrap(), orig);

        let _ = fs::remove_dir_all(&root);
    }
}
