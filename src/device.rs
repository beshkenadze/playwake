use std::sync::OnceLock;

pub struct Device {
    pub id: &'static str,
    pub name: &'static str,
}

pub fn current() -> &'static Device {
    static DEVICE: OnceLock<Device> = OnceLock::new();
    DEVICE.get_or_init(|| {
        let read = |field| std::fs::read_to_string(format!("/sys/class/dmi/id/{field}"))
            .unwrap_or_default();
        identify(&read("sys_vendor"), &read("product_name"), &read("product_version"))
    })
}

fn identify(vendor: &str, product: &str, version: &str) -> Device {
    let vendor = vendor.trim().to_ascii_lowercase();
    let model = format!("{product} {version}").to_ascii_lowercase();
    let is = |names: &[&str]| names.iter().any(|name| model.contains(name));
    let (id, name) = match vendor.as_str() {
        "valve" if is(&["galileo", "steam deck oled"]) => ("deck", "Steam Deck OLED"),
        "valve" if is(&["jupiter", "steam deck"]) => ("deck", "Steam Deck LCD"),
        "asustek computer inc." if is(&["rc72la"]) => ("ally", "ROG Ally X"),
        "asustek computer inc." if is(&["rc71l"]) => ("ally", "ROG Ally"),
        "lenovo" if is(&["8arp1", "8apu1", "8apu2", "legion go", "83e1"]) => {
            let name = if is(&["legion go s", "8arp1", "8apu2"]) { "Legion Go S" } else { "Legion Go" };
            ("legion", name)
        }
        _ => ("gamepad", "Gamepad"),
    };
    Device { id, name }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_known_handhelds_and_falls_back_for_other_hardware() {
        for (vendor, model, version, expected) in [
            ("ASUSTeK COMPUTER INC.\n", "ROG Ally RC71L_RC71L", "1.0", "ROG Ally"),
            ("ASUSTeK COMPUTER INC.", "RC72LA", "", "ROG Ally X"),
            ("Valve", "Jupiter", "", "Steam Deck LCD"),
            ("Valve", "Galileo", "", "Steam Deck OLED"),
            ("LENOVO", "83E1", "Legion Go 8APU1", "Legion Go"),
            ("LENOVO", "", "Legion Go S 8ARP1", "Legion Go S"),
            ("", "", "", "Gamepad"),
        ] {
            assert_eq!(identify(vendor, model, version).name, expected);
        }
    }
}
