use serde::{Deserialize, Serialize};
use schemars::JsonSchema;
use chrono::DateTime;
use chrono::Utc;


#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ActionPlan {
    pub goal:String,
    pub steps:Vec<ActionStep>,
    pub difficulty:Difficulty,
    pub estimated_minutes:u32,
    pub status: PlanStatus,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}



#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ActionStep {
    pub index: u8,
    pub status: StepStatus,
    pub description: String,
    pub tool_hint: Option<String>
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub enum StepStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub enum Difficulty {
    Easy,
    Medium,
    Hard,
}
