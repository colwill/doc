//! What each vendor offers. Every vendor makes object storage, which is cheap, quick and leaves
//! nothing running, and a virtual machine, which is what a service is usually stood up on and so
//! what a name in DNS points at. Adding a type here and in its vendor's adapter is all another
//! needs.

use serde_json::{Value, json};

pub const VENDORS: [(&str, &str); 4] =
    [("linode", "Linode"), ("aws", "AWS"), ("gcp", "GCP"), ("azure", "Azure")];

/// The type every vendor makes a machine under, which the adapters and the DNS record both read.
pub const VM: &str = "vm";

pub struct ResourceType {
    pub vendor: &'static str,
    pub name: &'static str,
    pub title: &'static str,
    /// What a size is for this type, if it has one.
    pub size: Option<&'static str>,
    pub sizes: &'static [&'static str],
    pub regions: &'static [&'static str],
}

pub const TYPES: [ResourceType; 8] = [
    ResourceType {
        vendor: "linode",
        name: "bucket",
        title: "Object Storage bucket",
        size: None,
        sizes: &[],
        regions: &["us-east", "us-southeast", "eu-central", "gb-lon", "ap-south"],
    },
    ResourceType {
        vendor: "linode",
        name: VM,
        title: "Linode instance",
        size: Some("plan"),
        sizes: &["g6-nanode-1", "g6-standard-1", "g6-standard-2", "g6-standard-4"],
        regions: &["us-east", "us-southeast", "eu-central", "gb-lon", "ap-south"],
    },
    ResourceType {
        vendor: "aws",
        name: "bucket",
        title: "S3 bucket",
        size: None,
        sizes: &[],
        regions: &["eu-west-2", "eu-west-1", "us-east-1", "us-west-2"],
    },
    ResourceType {
        vendor: "aws",
        name: VM,
        title: "EC2 instance",
        size: Some("instance type"),
        sizes: &["t3.micro", "t3.small", "t3.medium", "t3.large"],
        regions: &["eu-west-2", "eu-west-1", "us-east-1", "us-west-2"],
    },
    ResourceType {
        vendor: "gcp",
        name: "bucket",
        title: "Cloud Storage bucket",
        size: Some("storage class"),
        sizes: &["STANDARD", "NEARLINE", "COLDLINE", "ARCHIVE"],
        regions: &["europe-west2", "europe-west1", "us-central1", "us-east1"],
    },
    // Compute Engine places a machine in a zone rather than a region, so these are zones.
    ResourceType {
        vendor: "gcp",
        name: VM,
        title: "Compute Engine instance",
        size: Some("machine type"),
        sizes: &["e2-micro", "e2-small", "e2-medium", "e2-standard-2"],
        regions: &["europe-west2-a", "europe-west1-b", "us-central1-a", "us-east1-b"],
    },
    ResourceType {
        vendor: "azure",
        name: "storage-account",
        title: "Storage account",
        size: Some("redundancy"),
        sizes: &["Standard_LRS", "Standard_ZRS", "Standard_GRS"],
        regions: &["uksouth", "ukwest", "westeurope", "eastus"],
    },
    ResourceType {
        vendor: "azure",
        name: VM,
        title: "Virtual machine",
        size: Some("size"),
        sizes: &["Standard_B1s", "Standard_B2s", "Standard_B2ms", "Standard_D2s_v5"],
        regions: &["uksouth", "ukwest", "westeurope", "eastus"],
    },
];

pub fn vendor_name(vendor: &str) -> &'static str {
    VENDORS.iter().find(|(id, _)| *id == vendor).map_or("Unknown", |(_, name)| name)
}

pub fn resource_type(vendor: &str, name: &str) -> Option<&'static ResourceType> {
    TYPES.iter().find(|kind| kind.vendor == vendor && kind.name == name)
}

pub fn shown() -> Value {
    json!(
        TYPES
            .iter()
            .map(|kind| json!({
                "vendor": kind.vendor,
                "vendor_name": vendor_name(kind.vendor),
                "type": kind.name,
                "title": kind.title,
                "size": kind.size,
                "sizes": kind.sizes,
                "regions": kind.regions,
            }))
            .collect::<Vec<_>>()
    )
}
