//! Built-in sample data. Values are synthetic and never fetched over the network.
use chrono::{DateTime, SecondsFormat, Utc};

pub(super) fn sample_variable(name: &str, now: DateTime<Utc>) -> Option<String> {
    let random = uuid::Uuid::new_v4();
    let bytes = random.as_bytes();
    let number = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let pick = |values: &[&str]| values[number as usize % values.len()].to_string();
    let first = [
        "Alex", "Morgan", "Jordan", "Sam", "Taylor", "Jamie", "Robin", "Casey",
    ];
    let last = [
        "Bennett", "Rivera", "Patel", "Kim", "Martin", "Wilson", "Chen", "Garcia",
    ];
    let full = format!(
        "{} {}",
        first[bytes[0] as usize % first.len()],
        last[bytes[1] as usize % last.len()]
    );
    let token = random.simple().to_string();
    Some(match name {
        "$randomBoolean" => bytes[0].is_multiple_of(2).to_string(),
        "$randomAlphaNumeric" => char::from(
            b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ"
                [bytes[0] as usize % 62],
        )
        .to_string(),
        "$randomFirstName" => pick(&first),
        "$randomLastName" => pick(&last),
        "$randomFullName" => full,
        "$randomNamePrefix" => pick(&["Dr.", "Ms.", "Mr.", "Mx."]),
        "$randomNameSuffix" => pick(&["Jr.", "Sr.", "II", "III"]),
        "$randomEmail" | "$randomExampleEmail" => format!("user.{}@example.com", &token[..12]),
        "$randomUserName" => format!("user_{}", &token[..12]),
        "$randomPassword" => token[..15].to_string(),
        "$randomDomainName" => format!("sample-{}.test", &token[..10]),
        "$randomDomainWord" => format!("sample{}", &token[..10]),
        "$randomDomainSuffix" => pick(&["com", "net", "org"]),
        "$randomUrl" => format!("https://sample-{}.test", &token[..10]),
        "$randomLocale" => pick(&["en", "fr", "de", "es", "pt", "ja", "ko", "it"]),
        "$randomProtocol" => pick(&["http", "https"]),
        "$randomIP" => format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3]),
        "$randomIPV6" => std::net::Ipv6Addr::from(*bytes).to_string(),
        "$randomMACAddress" => bytes[..6]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
        "$randomSemver" => format!("{}.{}.{}", bytes[0] % 20, bytes[1] % 20, bytes[2] % 20),
        "$randomColor" => pick(&["red", "blue", "green", "purple", "orange", "black", "white"]),
        "$randomHexColor" => format!("#{}", &token[..6]),
        "$randomAbbreviation" => pick(&["HTTP", "SQL", "JSON", "XML", "CSS"]),
        "$randomPhoneNumber" => format!(
            "{:03}-{:03}-{:04}",
            number % 1000,
            (number / 1000) % 1000,
            u16::from_be_bytes([bytes[4], bytes[5]]) % 10000
        ),
        "$randomPhoneNumberExt" => format!(
            "{:02}-{:03}-{:03}-{:04}",
            bytes[6] % 100,
            number % 1000,
            (number / 1000) % 1000,
            u16::from_be_bytes([bytes[4], bytes[5]]) % 10000
        ),
        "$randomCity" => pick(&[
            "Riverton",
            "Lakeview",
            "Brookfield",
            "Fairview",
            "Oakdale",
            "Hillcrest",
        ]),
        "$randomStreetName" => pick(&["Maple Street", "Oak Avenue", "Cedar Road", "River Lane"]),
        "$randomStreetAddress" => format!(
            "{} {}",
            number % 9999 + 1,
            ["Maple Street", "Oak Avenue", "Cedar Road", "River Lane"][bytes[5] as usize % 4]
        ),
        "$randomCountry" => pick(&[
            "Canada",
            "France",
            "Germany",
            "Japan",
            "Australia",
            "Brazil",
        ]),
        "$randomCountryCode" => pick(&["CA", "FR", "DE", "JP", "AU", "BR"]),
        "$randomLatitude" => format!("{:.4}", number as f64 / u32::MAX as f64 * 180.0 - 90.0),
        "$randomLongitude" => format!("{:.4}", number as f64 / u32::MAX as f64 * 360.0 - 180.0),
        "$randomDateFuture" | "$randomDatePast" | "$randomDateRecent" => {
            let seconds = i64::from(
                number
                    % if name == "$randomDateRecent" {
                        86400
                    } else {
                        31536000
                    },
            ) + 1;
            let date = if name == "$randomDateFuture" {
                now + chrono::Duration::seconds(seconds)
            } else {
                now - chrono::Duration::seconds(seconds)
            };
            date.to_rfc3339_opts(SecondsFormat::Millis, true)
        }
        "$randomMonth" => pick(&[
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ]),
        "$randomWeekday" => pick(&[
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ]),
        "$randomCompanyName" => format!("{} Labs", pick(&last)),
        "$randomCompanySuffix" => pick(&["Inc", "LLC", "Group", "Ltd"]),
        "$randomJobArea" => pick(&["Engineering", "Operations", "Sales", "Finance"]),
        "$randomJobDescriptor" => pick(&["Senior", "Lead", "Principal", "Regional"]),
        "$randomJobType" => pick(&["Engineer", "Manager", "Coordinator", "Analyst"]),
        "$randomJobTitle" => pick(&[
            "Senior Engineer",
            "Operations Manager",
            "Financial Analyst",
            "Sales Coordinator",
        ]),
        "$randomBankAccount" => format!("{:08}", number % 100000000),
        "$randomCreditCardMask" => format!("{:04}", number % 10000),
        "$randomBankAccountName" => {
            pick(&["Checking Account", "Savings Account", "Home Loan Account"])
        }
        "$randomTransactionType" => pick(&["invoice", "payment", "deposit", "withdrawal"]),
        "$randomCurrencyCode" => pick(&["USD", "EUR", "GBP", "JPY", "CAD"]),
        "$randomCurrencyName" => pick(&[
            "US Dollar",
            "Euro",
            "Pound Sterling",
            "Yen",
            "Canadian Dollar",
        ]),
        "$randomCurrencySymbol" => pick(&["$", "€", "£", "¥"]),
        "$randomDatabaseColumn" => pick(&["id", "name", "createdAt", "updatedAt"]),
        "$randomDatabaseType" => pick(&["integer", "text", "boolean", "varchar"]),
        "$randomDatabaseEngine" => pick(&["InnoDB", "MyISAM", "Memory"]),
        "$randomDatabaseCollation" => pick(&["utf8_general_ci", "utf8_bin", "utf8_unicode_ci"]),
        "$randomFileName" | "$randomCommonFileName" => format!("sample-{}.json", &token[..8]),
        "$randomFileExt" | "$randomCommonFileExt" => pick(&["json", "txt", "png", "html", "csv"]),
        "$randomMimeType" => pick(&["application/json", "text/plain", "image/png", "text/html"]),
        "$randomFileType" | "$randomCommonFileType" => {
            pick(&["application", "text", "image", "audio"])
        }
        "$randomDirectoryPath" => format!("/samples/{}", &token[..8]),
        "$randomFilePath" => format!("/samples/{}.json", &token[..8]),
        "$randomWord" | "$randomNoun" => {
            pick(&["river", "forest", "cloud", "garden", "ocean", "mountain"])
        }
        "$randomAdjective" => pick(&["bright", "quiet", "green", "gentle", "clear"]),
        "$randomVerb" => pick(&["walk", "read", "write", "build", "explore"]),
        "$randomWords" => {
            format!("{} {} {}", pick(&first), pick(&last), &token[..4])
        }
        "$randomPhrase" => pick(&[
            "A quiet river flows through the valley.",
            "The morning sky is clear and bright.",
            "Green trees surround the peaceful garden.",
        ]),
        "$randomIngverb" => pick(&["walking", "reading", "writing", "building", "exploring"]),
        "$randomPrice" => format!("{:.2}", (number % 100001) as f64 / 100.),
        "$randomProduct" => pick(&["Shirt", "Chair", "Book", "Table", "Shoes"]),
        "$randomProductAdjective" => pick(&["Handmade", "Modern", "Practical", "Classic"]),
        "$randomProductMaterial" => pick(&["Cotton", "Steel", "Wood", "Leather"]),
        "$randomProductName" => pick(&[
            "Modern Wooden Chair",
            "Classic Cotton Shirt",
            "Handmade Leather Shoes",
        ]),
        "$randomDepartment" => pick(&["Books", "Clothing", "Electronics", "Home", "Tools"]),
        "$randomBsAdjective" | "$randomCatchPhraseAdjective" => {
            pick(&["Adaptive", "Connected", "Integrated", "Open"])
        }
        "$randomBsBuzz" => pick(&["connect", "build", "extend", "integrate"]),
        "$randomBsNoun" | "$randomCatchPhraseNoun" => {
            pick(&["systems", "networks", "platforms", "applications"])
        }
        "$randomCatchPhraseDescriptor" => pick(&["distributed", "collaborative", "interactive"]),
        "$randomBs" | "$randomCatchPhrase" => pick(&[
            "Connect distributed systems",
            "Build collaborative networks",
            "Extend interactive platforms",
        ]),
        "$randomUserAgent" => format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/{}.0.0.0 Safari/537.36",
            100 + bytes[0] % 40
        ),
        "$randomBankAccountBic" => format!(
            "{}GB2L",
            (0..4)
                .map(|i| char::from(b'A' + bytes[i] % 26))
                .collect::<String>()
        ),
        "$randomBankAccountIban" => {
            let account = format!(
                "{:018}",
                u64::from_be_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
                ]) % 1_000_000_000_000_000_000
            );
            let checksum_input = format!("{account}131400");
            let remainder = checksum_input
                .bytes()
                .fold(0u32, |r, b| (r * 10 + u32::from(b - b'0')) % 97);
            format!("DE{:02}{account}", 98 - remainder)
        }
        "$randomBitcoin" => {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(bytes);
            let mut payload = vec![0];
            payload.extend_from_slice(&digest[..20]);
            let checksum = Sha256::digest(Sha256::digest(&payload));
            payload.extend_from_slice(&checksum[..4]);
            let alphabet = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
            let mut digits = vec![0usize];
            for byte in &payload {
                let mut carry = usize::from(*byte);
                for digit in &mut digits {
                    carry += *digit * 256;
                    *digit = carry % 58;
                    carry /= 58;
                }
                while carry > 0 {
                    digits.push(carry % 58);
                    carry /= 58;
                }
            }
            let mut address = "1".repeat(payload.iter().take_while(|byte| **byte == 0).count());
            address.extend(
                digits
                    .iter()
                    .rev()
                    .map(|digit| char::from(alphabet[*digit])),
            );
            address
        }
        "$randomImageDataUri" => format!(
            "data:image/svg+xml,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%20width%3D%22640%22%20height%3D%22480%22%3E%3Crect%20width%3D%22640%22%20height%3D%22480%22%20fill%3D%22%23{}%22%2F%3E%3C%2Fsvg%3E",
            &token[..6]
        ),
        "$randomAvatarImage"
        | "$randomImageUrl"
        | "$randomAbstractImage"
        | "$randomAnimalsImage"
        | "$randomBusinessImage"
        | "$randomCatsImage"
        | "$randomCityImage"
        | "$randomFoodImage"
        | "$randomNightlifeImage"
        | "$randomFashionImage"
        | "$randomPeopleImage"
        | "$randomNatureImage"
        | "$randomSportsImage"
        | "$randomTransportImage" => {
            format!("https://picsum.photos/seed/{}/640/480", &token[..12])
        }
        "$randomLoremWord" => pick(&["lorem", "ipsum", "dolor", "sit", "amet", "consectetur"]),
        "$randomLoremWords" => pick(&[
            "lorem ipsum dolor",
            "sit amet consectetur",
            "adipiscing elit sed",
        ]),
        "$randomLoremSlug" => pick(&[
            "lorem-ipsum-dolor",
            "sit-amet-consectetur",
            "adipiscing-elit-sed",
        ]),
        "$randomLoremSentence" => pick(&[
            "Lorem ipsum dolor sit amet.",
            "Consectetur adipiscing elit.",
            "Sed do eiusmod tempor incididunt.",
        ]),
        "$randomLoremSentences"
        | "$randomLoremParagraph"
        | "$randomLoremText"
        | "$randomLoremLines"
        | "$randomLoremParagraphs" => {
            let sentences = [
                "Lorem ipsum dolor sit amet.",
                "Consectetur adipiscing elit.",
                "Sed do eiusmod tempor incididunt.",
            ];
            let count = if name == "$randomLoremParagraphs" {
                3
            } else {
                2 + usize::from(bytes[0] % 5)
            };
            let separator = if name == "$randomLoremLines" {
                "\n"
            } else if name == "$randomLoremParagraphs" {
                "\n\n"
            } else {
                " "
            };
            (0..count)
                .map(|i| sentences[(i + usize::from(bytes[1])) % sentences.len()])
                .collect::<Vec<_>>()
                .join(separator)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sample_types_are_valid_and_unknown_names_remain_errors() {
        let now = Utc::now();
        for _ in 0..32 {
            sample_variable("$randomBoolean", now)
                .unwrap()
                .parse::<bool>()
                .unwrap();
            sample_variable("$randomIP", now)
                .unwrap()
                .parse::<std::net::Ipv4Addr>()
                .unwrap();
            sample_variable("$randomIPV6", now)
                .unwrap()
                .parse::<std::net::Ipv6Addr>()
                .unwrap();
            assert!(
                sample_variable("$randomEmail", now)
                    .unwrap()
                    .ends_with("@example.com")
            );
            assert_eq!(sample_variable("$randomPassword", now).unwrap().len(), 15);
            assert!(
                (-90.0..=90.0).contains(
                    &sample_variable("$randomLatitude", now)
                        .unwrap()
                        .parse::<f64>()
                        .unwrap()
                )
            );
            assert!(
                DateTime::parse_from_rfc3339(&sample_variable("$randomDatePast", now).unwrap())
                    .unwrap()
                    < now
            );
            assert!(
                DateTime::parse_from_rfc3339(&sample_variable("$randomDateFuture", now).unwrap())
                    .unwrap()
                    > now
            );
        }
        assert_eq!(sample_variable("$randomUnsupported", now), None);
    }
}
