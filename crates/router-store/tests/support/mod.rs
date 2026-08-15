//! Shared test doubles. Each test binary that includes this module uses
//! only part of it, so unused-item warnings here are expected.
#![allow(dead_code)]

pub mod dynamodb_mock;
pub mod s3_mock;

/// The SDK refuses to sign without credentials and a region. These are
/// never checked by the mocks; they just have to exist.
pub fn fake_aws_env() {
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
        std::env::set_var("AWS_REGION", "us-east-1");
        std::env::set_var("AWS_EC2_METADATA_DISABLED", "true");
    }
}
