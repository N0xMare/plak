use plakous::auth::SecretString;

fn main() {
    let secret = SecretString::from("not-a-real-secret");
    let _ = secret.expose();
}
