//! Azure, through the Resource Manager REST API as a service principal: storage accounts with a
//! redundancy SKU, and virtual machines with the network a machine needs to be reachable, all in
//! one resource group and tagged with their DOC request, team and expiry.
//! `DOC_INFRA_AZURE_LOGIN` and `DOC_INFRA_AZURE_API` point it at a sandbox.

use std::time::Duration;

use async_trait::async_trait;
use doc_plugin_sdk::protocol::Secret;
use serde_json::{Value, json};

use super::{
    Adapter, Made, Wanted, client, credential, needed, offered, refused, setting, sized,
    unknown_password,
};
use crate::catalog::VM;

const STORAGE_API: &str = "2023-05-01";
const NETWORK_API: &str = "2024-05-01";
const COMPUTE_API: &str = "2024-07-01";

/// What a machine runs when the settings do not say, as publisher:offer:sku:version.
const IMAGE: &str = "Canonical:ubuntu-24_04-lts:server:latest";
/// The network a machine joins when the settings do not say, made on first use.
const NETWORK: &str = "doc-network";
const SUBNET: &str = "doc-subnet";
const NETWORK_RANGE: &str = "10.60.0.0/16";
const SUBNET_RANGE: &str = "10.60.0.0/24";
const ADMIN: &str = "docadmin";

/// How long teardown waits for one resource to go before giving up, which Azure needs because a
/// machine's card cannot be deleted until the machine itself has finished going.
const GONE_TRIES: usize = 24;
const GONE_WAIT: Duration = Duration::from_secs(5);

pub struct Azure {
    tenant: String,
    client_id: String,
    secret: Secret<String>,
    subscription: String,
    group: String,
    login: String,
    api: String,
    http: reqwest::Client,
}

impl Azure {
    pub fn from_settings() -> Result<Self, String> {
        let base = |name: &str, fallback: &str| {
            setting(name).unwrap_or_else(|| fallback.into()).trim_end_matches('/').to_string()
        };
        Ok(Self {
            tenant: needed("DOC_INFRA_AZURE_TENANT", "Azure")?,
            client_id: needed("DOC_INFRA_AZURE_CLIENT_ID", "Azure")?,
            secret: credential("DOC_INFRA_AZURE_CLIENT_SECRET", "Azure")?,
            subscription: needed("DOC_INFRA_AZURE_SUBSCRIPTION", "Azure")?,
            group: needed("DOC_INFRA_AZURE_RESOURCE_GROUP", "Azure")?,
            login: base("DOC_INFRA_AZURE_LOGIN", "https://login.microsoftonline.com"),
            api: base("DOC_INFRA_AZURE_API", "https://management.azure.com"),
            http: client()?,
        })
    }

    async fn token(&self) -> Result<Secret<String>, String> {
        let form = [
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.secret.expose().as_str()),
            ("scope", "https://management.azure.com/.default"),
        ];
        let answer = self
            .http
            .post(format!("{}/{}/oauth2/v2.0/token", self.login, self.tenant))
            .form(&form)
            .send()
            .await
            .map_err(|err| format!("Azure could not be reached: {err}"))?;
        if !answer.status().is_success() {
            return Err(refused("Azure's token service", answer).await);
        }
        let body: Value =
            answer.json().await.map_err(|err| format!("Azure's token answer: {err}"))?;
        body["access_token"]
            .as_str()
            .map(|token| Secret::new(token.to_string()))
            .ok_or_else(|| "Azure gave no token".into())
    }

    /// One resource's ID in the plugin's resource group, which is both where it is asked for and
    /// how another resource refers to it.
    fn id(&self, kind: &str, name: &str) -> String {
        format!(
            "/subscriptions/{}/resourceGroups/{}/providers/{kind}/{name}",
            self.subscription, self.group
        )
    }

    fn at(&self, kind: &str, name: &str, version: &str) -> String {
        format!("{}{}?api-version={version}", self.api, self.id(kind, name))
    }

    fn url(&self, name: &str) -> String {
        self.at("Microsoft.Storage/storageAccounts", name, STORAGE_API)
    }

    /// Reads a resource, answering `None` where Azure has none by that name.
    async fn read(&self, token: &Secret<String>, url: &str) -> Result<Option<Value>, String> {
        let answer = self
            .http
            .get(url)
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|err| format!("Azure could not be reached: {err}"))?;
        match answer.status().as_u16() {
            404 => Ok(None),
            200 => answer.json().await.map(Some).map_err(|err| format!("Azure's answer: {err}")),
            _ => Err(refused("Azure", answer).await),
        }
    }

    /// Asks for a resource to exist as described, answering what Azure said about it.
    async fn put(&self, token: &Secret<String>, url: &str, body: &Value) -> Result<Value, String> {
        let answer = self
            .http
            .put(url)
            .bearer_auth(token.expose())
            .json(body)
            .send()
            .await
            .map_err(|err| format!("Azure could not be reached: {err}"))?;
        match answer.status().as_u16() {
            // 202 means Azure took it on and is still building; its answer has no body, so
            // what was asked for stands in until the next sweep looks again.
            200..=202 => Ok(answer.json().await.unwrap_or_else(|_| body.clone())),
            _ => Err(refused("Azure", answer).await),
        }
    }

    async fn remove(&self, token: &Secret<String>, url: &str) -> Result<(), String> {
        let answer = self
            .http
            .delete(url)
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(|err| format!("Azure could not be reached: {err}"))?;
        match answer.status().as_u16() {
            200 | 202 | 204 | 404 => Ok(()),
            _ => Err(refused("Azure", answer).await),
        }
    }

    /// Waits until Azure no longer has the resource, because the next one cannot go until it has.
    async fn waited(&self, token: &Secret<String>, url: &str, what: &str) -> Result<(), String> {
        for _ in 0..GONE_TRIES {
            if self.read(token, url).await?.is_none() {
                return Ok(());
            }
            tokio::time::sleep(GONE_WAIT).await;
        }
        Err(format!("Azure was still deleting the {what} after a couple of minutes"))
    }

    fn made(name: &str, account: &Value) -> Made {
        let state = account["properties"]["provisioningState"].as_str().unwrap_or("Creating");
        Made {
            id: name.to_string(),
            detail: json!({
                "account": name,
                "location": account["location"],
                "sku": account["sku"]["name"],
                "state": state,
                "blob": account["properties"]["primaryEndpoints"]["blob"],
                "tags": account["tags"],
            }),
            ready: state == "Succeeded",
        }
    }

    fn machine(&self, name: &str, vm: &Value, address: Option<&str>) -> Made {
        let state = vm["properties"]["provisioningState"].as_str().unwrap_or("Creating");
        Made {
            id: name.to_string(),
            detail: json!({
                "instance": name,
                "location": vm["location"],
                "size": vm["properties"]["hardwareProfile"]["vmSize"],
                "state": state,
                "address": address,
                "admin": vm["properties"]["osProfile"]["adminUsername"],
                "tags": vm["tags"],
            }),
            // Succeeded with an address is the first moment it answers for itself, and so the
            // first moment a name pointing at it is worth having.
            ready: state == "Succeeded" && address.is_some(),
        }
    }

    fn network_name() -> String {
        setting("DOC_INFRA_AZURE_NETWORK").unwrap_or_else(|| NETWORK.into())
    }

    fn card(name: &str) -> String {
        format!("{name}-nic")
    }

    fn public_ip(name: &str) -> String {
        format!("{name}-ip")
    }

    /// Makes the network the machines share, unless it is already there: DOC never writes over a
    /// network it did not make, since another subnet in it would go with it.
    async fn network(&self, token: &Secret<String>, region: &str) -> Result<String, String> {
        let network = Self::network_name();
        let kind = "Microsoft.Network/virtualNetworks";
        let subnet = format!("{}/subnets/{SUBNET}", self.id(kind, &network));
        if self.read(token, &self.at(kind, &network, NETWORK_API)).await?.is_some() {
            return Ok(subnet);
        }
        let body = json!({
            "location": region,
            "properties": {
                "addressSpace": { "addressPrefixes": [NETWORK_RANGE] },
                "subnets": [{ "name": SUBNET, "properties": { "addressPrefix": SUBNET_RANGE } }],
            },
        });
        self.put(token, &self.at(kind, &network, NETWORK_API), &body).await?;
        Ok(subnet)
    }

    async fn create_machine(&self, wanted: &Wanted) -> Result<Made, String> {
        let token = self.token().await?;
        let size = sized("Azure", wanted)?;
        let region = &wanted.region;
        let name = &wanted.name;
        let subnet = self.network(&token, region).await?;
        // A public address, so the machine is reachable at the name DOC gives it.
        let ip_kind = "Microsoft.Network/publicIPAddresses";
        let ip = Self::public_ip(name);
        let ip_body = json!({
            "location": region,
            "sku": { "name": "Standard" },
            "properties": { "publicIPAllocationMethod": "Static" },
            "tags": wanted.labels,
        });
        self.put(&token, &self.at(ip_kind, &ip, NETWORK_API), &ip_body).await?;
        let card_kind = "Microsoft.Network/networkInterfaces";
        let card = Self::card(name);
        let card_body = json!({
            "location": region,
            "properties": {
                "ipConfigurations": [{
                    "name": "primary",
                    "properties": {
                        "subnet": { "id": subnet },
                        "publicIPAddress": { "id": self.id(ip_kind, &ip) },
                        "privateIPAllocationMethod": "Dynamic",
                    },
                }],
            },
            "tags": wanted.labels,
        });
        self.put(&token, &self.at(card_kind, &card, NETWORK_API), &card_body).await?;
        let image = setting("DOC_INFRA_AZURE_IMAGE").unwrap_or_else(|| IMAGE.into());
        let parts: Vec<&str> = image.split(':').collect();
        let [publisher, offer, sku, version] = parts.as_slice() else {
            return Err(format!("{image} is not publisher:offer:sku:version"));
        };
        let admin = setting("DOC_INFRA_AZURE_ADMIN_USERNAME").unwrap_or_else(|| ADMIN.into());
        let mut os = json!({ "computerName": name, "adminUsername": admin });
        match setting("DOC_INFRA_AZURE_SSH_KEY") {
            Some(key) => {
                os["linuxConfiguration"] = json!({
                    "disablePasswordAuthentication": true,
                    "ssh": { "publicKeys": [{
                        "path": format!("/home/{admin}/.ssh/authorized_keys"),
                        "keyData": key.trim(),
                    }] },
                });
            }
            // Azure insists on a way in even where nobody is meant to use it; the password is
            // made, sent and forgotten, and the way in is the key in the settings.
            None => os["adminPassword"] = json!(unknown_password()),
        }
        let vm_kind = "Microsoft.Compute/virtualMachines";
        let vm_body = json!({
            "location": region,
            "tags": wanted.labels,
            "properties": {
                "hardwareProfile": { "vmSize": size },
                "storageProfile": {
                    "imageReference": {
                        "publisher": publisher, "offer": offer, "sku": sku, "version": version,
                    },
                    "osDisk": {
                        "createOption": "FromImage",
                        "deleteOption": "Delete",
                        "managedDisk": { "storageAccountType": "StandardSSD_LRS" },
                    },
                },
                "osProfile": os,
                "networkProfile": {
                    "networkInterfaces": [{
                        "id": self.id(card_kind, &card),
                        "properties": { "primary": true, "deleteOption": "Delete" },
                    }],
                },
            },
        });
        let vm = self.put(&token, &self.at(vm_kind, name, COMPUTE_API), &vm_body).await?;
        // The address is allocated already, even while the machine is still being built.
        let address = self.address(&token, name).await?;
        Ok(self.machine(name, &vm, address.as_deref()))
    }

    /// The machine's public address, once Azure has given it one.
    async fn address(&self, token: &Secret<String>, name: &str) -> Result<Option<String>, String> {
        let kind = "Microsoft.Network/publicIPAddresses";
        let at = self.at(kind, &Self::public_ip(name), NETWORK_API);
        let Some(ip) = self.read(token, &at).await? else { return Ok(None) };
        Ok(ip["properties"]["ipAddress"].as_str().map(str::to_string).filter(|at| !at.is_empty()))
    }

    async fn inspect_machine(&self, id: &str) -> Result<Option<Made>, String> {
        let token = self.token().await?;
        let at = self.at("Microsoft.Compute/virtualMachines", id, COMPUTE_API);
        let Some(vm) = self.read(&token, &at).await? else { return Ok(None) };
        let address = self.address(&token, id).await?;
        Ok(Some(self.machine(id, &vm, address.as_deref())))
    }

    /// Deletes the machine, then the card and the address it held, each once the last has gone:
    /// Azure refuses a card a machine is still on, and an address a card is still on.
    async fn delete_machine(&self, id: &str) -> Result<(), String> {
        let token = self.token().await?;
        let vm = self.at("Microsoft.Compute/virtualMachines", id, COMPUTE_API);
        self.remove(&token, &vm).await?;
        self.waited(&token, &vm, "machine").await?;
        let card = self.at("Microsoft.Network/networkInterfaces", &Self::card(id), NETWORK_API);
        self.remove(&token, &card).await?;
        self.waited(&token, &card, "network card").await?;
        let ip = self.at("Microsoft.Network/publicIPAddresses", &Self::public_ip(id), NETWORK_API);
        self.remove(&token, &ip).await
    }
}

#[async_trait]
impl Adapter for Azure {
    async fn create(&self, wanted: &Wanted) -> Result<Made, String> {
        offered("Azure", wanted, &["storage-account", VM])?;
        if wanted.kind == VM {
            return self.create_machine(wanted).await;
        }
        let token = self.token().await?;
        let account = json!({
            "location": wanted.region,
            "kind": "StorageV2",
            "sku": { "name": wanted.size.clone().unwrap_or_else(|| "Standard_LRS".into()) },
            "tags": wanted.labels,
            "properties": { "minimumTlsVersion": "TLS1_2", "allowBlobPublicAccess": false },
        });
        let made = self.put(&token, &self.url(&wanted.name), &account).await?;
        Ok(Self::made(&wanted.name, &made))
    }

    async fn inspect(&self, wanted: &Wanted, id: &str) -> Result<Option<Made>, String> {
        if wanted.kind == VM {
            return self.inspect_machine(id).await;
        }
        let token = self.token().await?;
        Ok(self.read(&token, &self.url(id)).await?.map(|account| Self::made(id, &account)))
    }

    async fn delete(&self, wanted: &Wanted, id: &str) -> Result<(), String> {
        if wanted.kind == VM {
            return self.delete_machine(id).await;
        }
        let token = self.token().await?;
        self.remove(&token, &self.url(id)).await
    }
}
