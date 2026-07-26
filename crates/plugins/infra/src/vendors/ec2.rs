//! EC2, through its Query API signed with Signature Version 4.
//!
//! The rest of AWS here goes through the official SDK, and this one does not: `aws-sdk-ec2` is the
//! largest crate AWS publishes, and the three calls a machine needs — run it, look at it, end it —
//! are a stable, tiny corner of an API that has not changed shape since 2016. Signing is thirty
//! lines and the answers are flat XML, so this reads the handful of elements it wants rather than
//! taking a dependency that costs minutes of every build.

use std::collections::BTreeMap;

use doc_plugin_sdk::protocol::Secret;
use ring::{digest, hmac};
use serde_json::json;

use super::{Made, Wanted, client, credential, needed, setting, sized};

const VERSION: &str = "2016-11-15";
const SERVICE: &str = "ec2";
const SIGNED: &str = "content-type;host;x-amz-date";
const FORM: &str = "application/x-www-form-urlencoded";

/// Amazon Linux 2023, which EC2 looks up per region itself, so one setting serves every region.
const IMAGE: &str =
    "resolve:ssm:/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64";

pub struct Ec2 {
    key: String,
    secret: Secret<String>,
    /// An endpoint for every region, where a sandbox stands in for EC2; otherwise Amazon's own.
    endpoint: Option<String>,
    http: reqwest::Client,
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, bytes))
}

fn sign(key: &[u8], message: &str) -> hmac::Tag {
    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), message.as_bytes())
}

/// The text of the first `<name>…</name>` in `xml`, which is all these answers need: every
/// element read here holds a scalar, and a run of one instance has one of each.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let from = xml.find(&open)? + open.len();
    let to = xml[from..].find(&close)? + from;
    Some(xml[from..to].trim())
}

fn text(xml: &str, name: &str) -> Option<String> {
    tag(xml, name).filter(|found| !found.is_empty()).map(str::to_string)
}

/// What AWS said went wrong, as its error XML puts it.
fn problem(status: u16, body: &str) -> String {
    match (tag(body, "Code"), tag(body, "Message")) {
        (Some(code), Some(message)) => format!("AWS answered {status}: {code}: {message}"),
        _ => format!("AWS answered {status}: {}", body.chars().take(300).collect::<String>()),
    }
}

/// Whether a refusal was AWS saying the instance is not one of its own, which for DOC means it
/// has gone: `problem` keeps the code AWS gave in the message.
fn unknown(refusal: &str) -> bool {
    refusal.contains("InvalidInstanceID")
}

impl Ec2 {
    pub fn from_settings() -> Result<Self, String> {
        Ok(Self {
            key: needed("DOC_INFRA_AWS_ACCESS_KEY_ID", "AWS")?,
            secret: credential("DOC_INFRA_AWS_SECRET_ACCESS_KEY", "AWS")?,
            endpoint: setting("DOC_INFRA_AWS_EC2_ENDPOINT"),
            http: client()?,
        })
    }

    fn url(&self, region: &str) -> String {
        match &self.endpoint {
            Some(endpoint) => endpoint.trim_end_matches('/').to_string(),
            None => format!("https://ec2.{region}.amazonaws.com"),
        }
    }

    /// One Query API call, signed as Signature Version 4 asks, answering its XML.
    async fn call(&self, region: &str, form: &[(String, String)]) -> Result<String, String> {
        // The serializer itself is dropped here: it is not `Send`, and the call awaits later.
        let body = {
            let mut writing = url::form_urlencoded::Serializer::new(String::new());
            writing.append_pair("Version", VERSION);
            for (name, value) in form {
                writing.append_pair(name, value);
            }
            writing.finish()
        };
        let url = self.url(region);
        let parsed =
            url::Url::parse(&url).map_err(|err| format!("{url} is not an EC2 endpoint: {err}"))?;
        let host = match parsed.port() {
            Some(port) => format!("{}:{port}", parsed.host_str().unwrap_or_default()),
            None => parsed.host_str().unwrap_or_default().to_string(),
        };
        let now = chrono::Utc::now();
        let moment = now.format("%Y%m%dT%H%M%SZ").to_string();
        let day = now.format("%Y%m%d").to_string();
        let path = match parsed.path() {
            "" => "/",
            path => path,
        };
        let canonical = format!(
            "POST\n{path}\n\ncontent-type:{FORM}\nhost:{host}\nx-amz-date:{moment}\n\n{SIGNED}\n{}",
            sha256_hex(body.as_bytes())
        );
        let scope = format!("{day}/{region}/{SERVICE}/aws4_request");
        let to_sign =
            format!("AWS4-HMAC-SHA256\n{moment}\n{scope}\n{}", sha256_hex(canonical.as_bytes()));
        let day_key = sign(format!("AWS4{}", self.secret.expose()).as_bytes(), &day);
        let region_key = sign(day_key.as_ref(), region);
        let service_key = sign(region_key.as_ref(), SERVICE);
        let signing_key = sign(service_key.as_ref(), "aws4_request");
        let signature = hex::encode(sign(signing_key.as_ref(), &to_sign).as_ref());
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={SIGNED}, Signature={signature}",
            self.key
        );
        let answer = self
            .http
            .post(&url)
            .header("content-type", FORM)
            .header("x-amz-date", &moment)
            .header("authorization", authorization)
            .body(body)
            .send()
            .await
            .map_err(|err| format!("AWS could not be reached: {err}"))?;
        let status = answer.status().as_u16();
        let text = answer.text().await.unwrap_or_default();
        match (200..300).contains(&status) {
            true => Ok(text),
            false => Err(problem(status, &text)),
        }
    }

    pub async fn create(&self, wanted: &Wanted) -> Result<Made, String> {
        let image = setting("DOC_INFRA_AWS_IMAGE").unwrap_or_else(|| IMAGE.into());
        let mut form = vec![
            ("Action".to_string(), "RunInstances".to_string()),
            ("ImageId".to_string(), image),
            ("InstanceType".to_string(), sized("AWS", wanted)?),
            ("MinCount".to_string(), "1".to_string()),
            ("MaxCount".to_string(), "1".to_string()),
            ("TagSpecification.1.ResourceType".to_string(), "instance".to_string()),
            ("TagSpecification.1.Tag.1.Key".to_string(), "Name".to_string()),
            ("TagSpecification.1.Tag.1.Value".to_string(), wanted.name.clone()),
        ];
        for (at, (key, value)) in wanted.labels.iter().enumerate() {
            let at = at + 2;
            form.push((format!("TagSpecification.1.Tag.{at}.Key"), key.clone()));
            form.push((format!("TagSpecification.1.Tag.{at}.Value"), value.clone()));
        }
        if let Some(pair) = setting("DOC_INFRA_AWS_KEY_PAIR") {
            form.push(("KeyName".to_string(), pair));
        }
        // Without a subnet the account's default VPC is used, which hands out a public address;
        // a subnet that is named has to be one that does the same for the machine to be reachable.
        if let Some(subnet) = setting("DOC_INFRA_AWS_SUBNET") {
            form.push(("SubnetId".to_string(), subnet));
        }
        if let Some(group) = setting("DOC_INFRA_AWS_SECURITY_GROUP") {
            form.push(("SecurityGroupId.1".to_string(), group));
        }
        let xml = self.call(&wanted.region, &form).await?;
        let id = text(&xml, "instanceId")
            .ok_or_else(|| "AWS started an instance but named none".to_string())?;
        Ok(made(&id, &xml, &wanted.region))
    }

    pub async fn inspect(&self, wanted: &Wanted, id: &str) -> Result<Option<Made>, String> {
        let form = vec![
            ("Action".to_string(), "DescribeInstances".to_string()),
            ("InstanceId.1".to_string(), id.to_string()),
        ];
        let xml = match self.call(&wanted.region, &form).await {
            Ok(xml) => xml,
            Err(err) if unknown(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        let made = made(id, &xml, &wanted.region);
        // A machine that has ended is still described for an hour or so; DOC treats it as gone.
        let state = made.detail["state"].as_str().unwrap_or_default();
        match matches!(state, "terminated" | "shutting-down") {
            true => Ok(None),
            false => Ok(Some(made)),
        }
    }

    pub async fn delete(&self, wanted: &Wanted, id: &str) -> Result<(), String> {
        let form = vec![
            ("Action".to_string(), "TerminateInstances".to_string()),
            ("InstanceId.1".to_string(), id.to_string()),
        ];
        match self.call(&wanted.region, &form).await {
            Ok(_) => Ok(()),
            Err(err) if unknown(&err) => Ok(()),
            Err(err) => Err(err),
        }
    }
}

/// An instance as DOC shows it, read from whichever answer described it.
fn made(id: &str, xml: &str, region: &str) -> Made {
    let state = xml
        .find("<instanceState>")
        .and_then(|from| tag(&xml[from..], "name"))
        .unwrap_or("pending")
        .to_string();
    let address = text(xml, "ipAddress");
    // Running with an address is the first moment it answers for itself, and so the first moment
    // a name pointing at it is worth having.
    let ready = state == "running" && address.is_some();
    let tags: BTreeMap<String, String> = tagged(xml);
    Made {
        id: id.to_string(),
        detail: json!({
            "instance": id,
            "region": region,
            "type": text(xml, "instanceType"),
            "image": text(xml, "imageId"),
            "state": state,
            "address": address,
            "hostname": text(xml, "dnsName"),
            "private_address": text(xml, "privateIpAddress"),
            "tags": tags,
        }),
        ready,
    }
}

/// The instance's tags, which the answer gives as a `tagSet` of key and value pairs.
fn tagged(xml: &str) -> BTreeMap<String, String> {
    let Some(from) = xml.find("<tagSet>") else { return BTreeMap::new() };
    let Some(to) = xml[from..].find("</tagSet>").map(|to| to + from) else {
        return BTreeMap::new();
    };
    xml[from..to]
        .split("<item>")
        .skip(1)
        .filter_map(|item| Some((tag(item, "key")?.to_string(), tag(item, "value")?.to_string())))
        .collect()
}
