//! AWS: S3 buckets through the official SDK, and EC2 instances through `super::ec2`, both tagged
//! with their DOC request, team and expiry. `DOC_INFRA_AWS_ENDPOINT` points the SDK at an
//! S3-compatible sandbox instead.

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::types::{
    BucketLocationConstraint, CreateBucketConfiguration, Delete, ObjectIdentifier, Tag, Tagging,
};
use doc_plugin_sdk::protocol::Secret;
use serde_json::json;

use super::ec2::Ec2;
use super::{Adapter, Made, Wanted, credential, needed, offered, setting};
use crate::catalog::VM;

pub struct Aws {
    key: String,
    secret: Secret<String>,
    endpoint: Option<String>,
    ec2: Ec2,
}

impl Aws {
    pub fn from_settings() -> Result<Self, String> {
        Ok(Self {
            key: needed("DOC_INFRA_AWS_ACCESS_KEY_ID", "AWS")?,
            secret: credential("DOC_INFRA_AWS_SECRET_ACCESS_KEY", "AWS")?,
            endpoint: setting("DOC_INFRA_AWS_ENDPOINT"),
            ec2: Ec2::from_settings()?,
        })
    }

    fn client(&self, region: &str) -> Client {
        let credentials =
            Credentials::new(&self.key, self.secret.expose(), None, None, "doc-infra");
        let mut config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .credentials_provider(credentials);
        if let Some(endpoint) = &self.endpoint {
            config = config.endpoint_url(endpoint).force_path_style(true);
        }
        Client::from_conf(config.build())
    }
}

fn failed(what: &str, err: &impl std::error::Error) -> String {
    format!("AWS could not {what}: {}", DisplayErrorContext(err))
}

fn made(wanted: &Wanted, tags: &[(String, String)]) -> Made {
    let region = &wanted.region;
    Made {
        id: wanted.name.clone(),
        detail: json!({
            "bucket": wanted.name,
            "region": region,
            "endpoint": format!("https://{}.s3.{region}.amazonaws.com", wanted.name),
            "tags": tags.iter().map(|(key, value)| json!({ "key": key, "value": value })).collect::<Vec<_>>(),
        }),
        ready: true,
    }
}

#[async_trait]
impl Adapter for Aws {
    async fn create(&self, wanted: &Wanted) -> Result<Made, String> {
        offered("AWS", wanted, &["bucket", VM])?;
        if wanted.kind == VM {
            return self.ec2.create(wanted).await;
        }
        let client = self.client(&wanted.region);
        let mut asked = client.create_bucket().bucket(&wanted.name);
        if wanted.region != "us-east-1" {
            let place = BucketLocationConstraint::from(wanted.region.as_str());
            asked = asked.create_bucket_configuration(
                CreateBucketConfiguration::builder().location_constraint(place).build(),
            );
        }
        asked.send().await.map_err(|err| failed("make the bucket", &err))?;
        let mut tags = Vec::new();
        for (key, value) in &wanted.labels {
            tags.push(
                Tag::builder()
                    .key(key)
                    .value(value)
                    .build()
                    .map_err(|err| failed("tag the bucket", &err))?,
            );
        }
        let tagging = Tagging::builder()
            .set_tag_set(Some(tags))
            .build()
            .map_err(|err| failed("tag the bucket", &err))?;
        client
            .put_bucket_tagging()
            .bucket(&wanted.name)
            .tagging(tagging)
            .send()
            .await
            .map_err(|err| failed("tag the bucket", &err))?;
        let labels: Vec<(String, String)> =
            wanted.labels.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        Ok(made(wanted, &labels))
    }

    async fn inspect(&self, wanted: &Wanted, id: &str) -> Result<Option<Made>, String> {
        if wanted.kind == VM {
            return self.ec2.inspect(wanted, id).await;
        }
        let client = self.client(&wanted.region);
        match client.head_bucket().bucket(id).send().await {
            Ok(_) => {}
            Err(err) if err.as_service_error().is_some_and(|err| err.is_not_found()) => {
                return Ok(None);
            }
            Err(err) if err.raw_response().is_some_and(|raw| raw.status().as_u16() == 404) => {
                return Ok(None);
            }
            Err(err) => return Err(failed("look at the bucket", &err)),
        }
        let tagged = client.get_bucket_tagging().bucket(id).send().await;
        let tags: Vec<(String, String)> = match tagged {
            Ok(answer) => answer
                .tag_set()
                .iter()
                .map(|tag| (tag.key().to_string(), tag.value().to_string()))
                .collect(),
            Err(_) => Vec::new(),
        };
        let current = Wanted { name: id.to_string(), ..wanted.clone() };
        Ok(Some(made(&current, &tags)))
    }

    async fn delete(&self, wanted: &Wanted, id: &str) -> Result<(), String> {
        if wanted.kind == VM {
            return self.ec2.delete(wanted, id).await;
        }
        let client = self.client(&wanted.region);
        loop {
            let listed = client
                .list_objects_v2()
                .bucket(id)
                .send()
                .await
                .map_err(|err| failed("list the bucket", &err))?;
            let keys: Vec<ObjectIdentifier> = listed
                .contents()
                .iter()
                .filter_map(|object| object.key())
                .filter_map(|key| ObjectIdentifier::builder().key(key).build().ok())
                .collect();
            if keys.is_empty() {
                break;
            }
            let delete = Delete::builder()
                .set_objects(Some(keys))
                .build()
                .map_err(|err| failed("empty the bucket", &err))?;
            client
                .delete_objects()
                .bucket(id)
                .delete(delete)
                .send()
                .await
                .map_err(|err| failed("empty the bucket", &err))?;
        }
        client
            .delete_bucket()
            .bucket(id)
            .send()
            .await
            .map_err(|err| failed("delete the bucket", &err))?;
        Ok(())
    }
}
