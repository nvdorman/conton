use super::*;

#[test]
fn device_code_prompt_renders_codebuddy_login_hint() {
    let prompt = device_code_prompt(
        "https://www.codebuddy.ai/login?platform=CLI&state=abc",
        "abcd1234",
    );

    assert!(prompt.contains("CodeBuddy"));
    assert!(prompt.contains("Conton"));
    assert!(prompt.contains("www.codebuddy.ai/login"));
    assert!(prompt.contains("abcd1234"));
}

#[test]
fn resolve_codebuddy_base_maps_openai_issuer_to_global() {
    assert_eq!(
        resolve_codebuddy_base("https://auth.openai.com"),
        CODEBUDDY_DEFAULT_BASE
    );
    assert_eq!(
        resolve_codebuddy_base("https://www.codebuddy.ai/"),
        "https://www.codebuddy.ai"
    );
}
