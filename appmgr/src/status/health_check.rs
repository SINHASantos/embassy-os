use chrono::{DateTime, Utc};
use emver::Version;
use serde::{Deserialize, Serialize};

use crate::action::ActionImplementation;
use crate::s9pk::manifest::PackageId;
use crate::volume::Volumes;
use crate::Error;

#[derive(Debug, Deserialize, Serialize)]
pub struct HealthCheck(ActionImplementation);
impl HealthCheck {
    pub async fn check(
        &self,
        pkg_id: &PackageId,
        pkg_version: &Version,
        volumes: &Volumes,
    ) -> Result<HealthCheckResult, Error> {
        let res = self
            .0
            .execute(pkg_id, pkg_version, volumes, None::<()>)
            .await?;
        Ok(HealthCheckResult {
            time: Utc::now(),
            result: match res {
                Ok(()) => HealthCheckResultVariant::Success,
                Err((59, _)) => HealthCheckResultVariant::Disabled,
                Err((_, error)) => HealthCheckResultVariant::Failure { error },
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HealthCheckResult {
    pub time: DateTime<Utc>,
    #[serde(flatten)]
    pub result: HealthCheckResultVariant,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
#[serde(tag = "result")]
pub enum HealthCheckResultVariant {
    Disabled,
    Success,
    Failure { error: String },
}
impl std::fmt::Display for HealthCheckResultVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HealthCheckResultVariant::Disabled => write!(f, "Disabled"),
            HealthCheckResultVariant::Success => write!(f, "Succeeded"),
            HealthCheckResultVariant::Failure { error } => write!(f, "Failed ({})", error),
        }
    }
}
