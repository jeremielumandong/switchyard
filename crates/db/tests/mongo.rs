//! MongoDB integration tests against the docker `mongo` service
//! (`docker compose -f docker/compose.yml up -d mongo`) or any server reachable with
//! `SWITCHYARD_MONGO_HOST`, `_PORT`, `_USER`, `_PASSWORD`.
//! Run with `cargo test -p switchyard-db --test mongo -- --ignored --test-threads 1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use secrecy::SecretString;
use switchyard_db::batch::{BatchList, ColumnMeta, DOCUMENT_COLUMN};
use switchyard_db::catalog::{CatalogChunk, IntrospectScope, ObjectKind};
use switchyard_db::driver::{DbConfig, DbSession, Driver};
use switchyard_db::error::{DbError, ErrorPosition};
use switchyard_db::mongo::MongoDriver;
use switchyard_db::stream::{Completion, ResultEvent};
use switchyard_db::value::{DataType, Engine, Value};

const DB: &str = "switchyard_it";

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn config() -> DbConfig {
    let mut cfg = DbConfig::new(
        Engine::MongoDb,
        env("SWITCHYARD_MONGO_HOST", "localhost"),
        DB,
    );
    cfg.port = env("SWITCHYARD_MONGO_PORT", "27017").parse().unwrap();
    cfg.user = env("SWITCHYARD_MONGO_USER", "switchyard");
    cfg.password = Some(SecretString::from(env(
        "SWITCHYARD_MONGO_PASSWORD",
        "switchyard",
    )));
    cfg.connect_timeout = Duration::from_secs(5);
    cfg
}

async fn session() -> Box<dyn DbSession> {
    MongoDriver.connect(&config(), None).await.expect("connect")
}

/// Every result set: (columns, rows), plus the completion.
async fn run(s: &mut dyn DbSession, sql: &str) -> (Vec<(Vec<ColumnMeta>, BatchList)>, Completion) {
    let mut stream = s.execute(sql, &[]).await.expect(sql);
    let mut sets: Vec<(Vec<ColumnMeta>, BatchList)> = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev.expect("event") {
            ResultEvent::Columns(c) => sets.push((c.to_vec(), BatchList::default())),
            ResultEvent::Rows(b) => sets.last_mut().expect("columns first").1.push(b),
            ResultEvent::Done(c) => return (sets, c),
            _ => {}
        }
    }
    panic!("no Done for {sql}")
}

fn cell(set: &(Vec<ColumnMeta>, BatchList), row: usize, col: &str) -> Value {
    let c = set.0.iter().position(|m| m.name == col).expect(col);
    set.1
        .cell(row, c)
        .map(|v| v.to_value(set.0[c].data_type))
        .expect("cell")
}

#[tokio::test]
#[ignore = "needs the docker mongo service"]
async fn crud_find_and_flatten() {
    let mut s = session().await;
    assert!(s.server_version().starts_with("MongoDB "));
    run(s.as_mut(), "db.people.drop()").await;
    let (sets, done) = run(
        s.as_mut(),
        "db.people.insertMany([
          { _id: 1, name: 'Ada', age: 36, address: { city: 'London' }, tags: ['math'], at: ISODate('2024-05-01T10:00:00Z') },
          { _id: 2, name: 'Bob', age: 41.5, address: { city: 'Paris' } },
          { _id: 3, name: 'Cy' }
        ])",
    )
    .await;
    assert_eq!(done.affected, Some(3));
    assert_eq!(sets[0].1.len(), 3);

    let (sets, _) = run(
        s.as_mut(),
        "db.people.find({ age: { $gte: 30 } }).sort({ age: -1 })",
    )
    .await;
    let set = &sets[0];
    let names: Vec<&str> = set.0.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "_id",
            "name",
            "age",
            "address.city",
            "tags",
            "at",
            DOCUMENT_COLUMN
        ]
    );
    assert_eq!(set.1.len(), 2);
    assert_eq!(cell(set, 0, "name"), Value::Text("Bob".into()));
    assert_eq!(cell(set, 0, "address.city"), Value::Text("Paris".into()));
    assert_eq!(cell(set, 1, "age"), Value::Float(36.0));
    assert_eq!(
        set.0.iter().find(|c| c.name == "at").map(|c| c.data_type),
        Some(DataType::TimestampTz)
    );

    let (_, done) = run(
        s.as_mut(),
        "db.people.updateMany({ age: { $exists: false } }, { $set: { age: 1 } })",
    )
    .await;
    assert_eq!(done.affected, Some(1));
    let (sets, _) = run(s.as_mut(), "db.people.countDocuments({})").await;
    assert_eq!(cell(&sets[0], 0, "count"), Value::Int(3));
    let (sets, _) = run(s.as_mut(), "db.people.distinct('address.city')").await;
    assert_eq!(sets[0].1.len(), 2);
    let (sets, _) = run(
        s.as_mut(),
        "db.people.aggregate([{ $group: { _id: null, n: { $sum: 1 } } }])",
    )
    .await;
    assert_eq!(cell(&sets[0], 0, "n"), Value::Int(3));
    let (_, done) = run(s.as_mut(), "db.people.deleteOne({ _id: 3 })").await;
    assert_eq!(done.affected, Some(1));
}

#[tokio::test]
#[ignore = "needs the docker mongo service"]
async fn streams_large_cursors_in_batches() {
    let mut s = session().await;
    run(s.as_mut(), "db.many.drop()").await;
    let docs: Vec<String> = (0..2500).map(|i| format!("{{ n: {i} }}")).collect();
    run(
        s.as_mut(),
        &format!("db.many.insertMany([{}])", docs.join(",")),
    )
    .await;
    let mut stream = s.execute("db.many.find({})", &[]).await.expect("find");
    let mut batches = 0;
    let mut rows = 0;
    while let Some(ev) = stream.next().await {
        if let ResultEvent::Rows(b) = ev.expect("event") {
            batches += 1;
            rows += b.len();
        }
    }
    assert_eq!(rows, 2500);
    assert!(batches >= 3, "{batches} batches");
}

#[tokio::test]
#[ignore = "needs the docker mongo service"]
async fn errors_use_and_catalog() {
    let mut s = session().await;
    let e = s
        .execute("db.people.find({ a: })", &[])
        .await
        .err()
        .expect("syntax error");
    assert_eq!(
        e.as_server().and_then(|x| x.position),
        // 1-based: the `}` where a value was expected.
        Some(ErrorPosition::Offset(21))
    );
    let e = s
        .execute("db.people.aggregate([{ $nope: 1 }])", &[])
        .await
        .err()
        .expect("server error");
    assert!(matches!(e, DbError::Server(_)), "{e:?}");

    run(s.as_mut(), "db.people.insertOne({ name: 'x' })").await;
    run(s.as_mut(), "db.people.createIndex({ name: 1 })").await;
    run(s.as_mut(), "db.adults.drop()").await;
    run(
        s.as_mut(),
        "db.createView('adults', 'people', [{ $match: { age: { $gte: 18 } } }])",
    )
    .await;

    let CatalogChunk::Schemas(schemas) = s.introspect(IntrospectScope::Schemas).await.unwrap()
    else {
        panic!("schemas")
    };
    assert!(schemas.iter().any(|x| x.name == DB));
    assert!(schemas.iter().any(|x| x.name == "admin" && x.is_system));
    let CatalogChunk::Objects(tables) = s
        .introspect(IntrospectScope::Objects {
            schema: DB.into(),
            kind: ObjectKind::Table,
        })
        .await
        .unwrap()
    else {
        panic!("objects")
    };
    assert!(tables.iter().any(|o| o.name == "people"));
    assert!(!tables.iter().any(|o| o.name == "adults"));
    let CatalogChunk::Objects(views) = s
        .introspect(IntrospectScope::Objects {
            schema: DB.into(),
            kind: ObjectKind::View,
        })
        .await
        .unwrap()
    else {
        panic!("views")
    };
    assert!(views.iter().any(|o| o.name == "adults"));
    let CatalogChunk::Detail(d) = s
        .introspect(IntrospectScope::Detail {
            schema: DB.into(),
            name: "people".into(),
            kind: ObjectKind::Table,
        })
        .await
        .unwrap()
    else {
        panic!("detail")
    };
    assert!(d.columns.iter().any(|c| c.name == "name"));
    assert!(d.indexes.iter().any(|i| i.name == "name_1"));
    assert!(d.ddl.contains("createIndex"), "{}", d.ddl);
    let CatalogChunk::Objects(hits) = s
        .introspect(IntrospectScope::Search {
            pattern: "PEOP".into(),
            limit: 10,
            include_system: false,
        })
        .await
        .unwrap()
    else {
        panic!("search")
    };
    assert!(hits.iter().any(|o| o.name == "people" && o.schema == DB));

    // `use` switches the database later statements run in.
    run(s.as_mut(), "use admin").await;
    let (sets, _) = run(s.as_mut(), "db.runCommand({ ping: 1 })").await;
    assert_eq!(sets.len(), 1);
    let (sets, _) = run(s.as_mut(), "show dbs").await;
    assert!(sets[0].1.len() >= 2);
}

#[tokio::test]
#[ignore = "needs the docker mongo service"]
async fn wrong_password_is_a_connect_error() {
    let mut cfg = config();
    cfg.password = Some(SecretString::from("nope".to_owned()));
    let e = MongoDriver
        .connect(&cfg, None)
        .await
        .err()
        .expect("refused");
    assert!(matches!(e, DbError::Connect(_)), "{e:?}");
}

#[tokio::test]
#[ignore = "needs the docker mongo service"]
async fn refused_sign_in_names_the_auth_database_that_works() {
    let mut s = session().await;
    // A user created in the connection's database, not in admin.
    if let Ok(mut st) = s
        .execute("db.runCommand({ dropUser: 'local_app' })", &[])
        .await
    {
        while st.next().await.is_some() {}
    }
    run(
        s.as_mut(),
        "db.runCommand({ createUser: 'local_app', pwd: 'Secr3t!', roles: [] })",
    )
    .await;
    let mut cfg = config();
    cfg.user = "local_app".into();
    cfg.password = Some(SecretString::from("Secr3t!".to_owned()));
    let e = MongoDriver
        .connect(&cfg, None)
        .await
        .err()
        .expect("refused against admin");
    let text = e.to_string();
    assert!(text.contains("(auth database admin)"), "{text}");
    assert!(
        text.contains(&format!("set Auth database to {DB}")),
        "{text}"
    );
    cfg.options.insert("auth_source".into(), DB.into());
    MongoDriver.connect(&cfg, None).await.expect("signs in");

    let mut cfg = config();
    cfg.password = Some(SecretString::from("nope".to_owned()));
    let text = MongoDriver
        .connect(&cfg, None)
        .await
        .err()
        .expect("refused")
        .to_string();
    assert!(text.contains("Check the password"), "{text}");
}

#[tokio::test]
#[ignore = "needs the docker mongo service"]
async fn grid_edits_by_id() {
    use switchyard_db::mongo::edit::{edit_target, row_delete, row_insert, row_update};
    let mut s = session().await;
    run(s.as_mut(), "db.edits.drop()").await;
    run(
        s.as_mut(),
        "db.edits.insertMany([
          { _id: ObjectId('507f1f77bcf86cd799439011'), name: 'Ada', n: NumberLong(5), zip: '02139', address: { city: 'London' } },
          { _id: 7, name: 'Bob', n: NumberLong(1), zip: '10001' }
        ])",
    )
    .await;
    let find = "db.edits.find({}).sort({ name: 1 })";
    let (sets, _) = run(s.as_mut(), find).await;
    let set = &sets[0];
    let table = edit_target(find, &set.0).unwrap();
    let doc = |row: usize| match cell(set, row, DOCUMENT_COLUMN) {
        Value::Json(s) => s,
        other => panic!("{other:?}"),
    };
    let text = |s: &str| Value::Text(s.into());

    // Ada: nested path, a long stays a long, a string of digits stays a string.
    let upd = row_update(
        &table,
        &set.0,
        &doc(0),
        &[
            ("address.city".into(), text("Paris")),
            ("n".into(), text("6")),
            ("zip".into(), text("94105")),
        ],
    )
    .unwrap();
    let (_, done) = run(s.as_mut(), &upd).await;
    assert_eq!(done.affected, Some(1));
    // The same update again matches its document though nothing changes.
    let (_, done) = run(s.as_mut(), &upd).await;
    assert_eq!(done.affected, Some(1));
    let (sets, _) = run(
        s.as_mut(),
        "db.edits.countDocuments({ _id: ObjectId('507f1f77bcf86cd799439011'), 'address.city': 'Paris', n: { $type: 'long' }, zip: { $type: 'string' } })",
    )
    .await;
    assert_eq!(cell(&sets[0], 0, "count"), Value::Int(1));

    // Bob (a numeric _id) is deleted, and a new document is inserted.
    let del = row_delete(&table, &doc(1)).unwrap();
    let (_, done) = run(s.as_mut(), &del).await;
    assert_eq!(done.affected, Some(1));
    let ins = row_insert(
        &table,
        &set.0,
        &[
            ("name".into(), text("Cy")),
            ("address.city".into(), text("Rome")),
        ],
    )
    .unwrap();
    let (_, done) = run(s.as_mut(), &ins).await;
    assert_eq!(done.affected, Some(1));
    let (sets, _) = run(s.as_mut(), "db.edits.find({}).sort({ name: 1 })").await;
    let names: Vec<Value> = (0..sets[0].1.len())
        .map(|r| cell(&sets[0], r, "name"))
        .collect();
    assert_eq!(names, [text("Ada"), text("Cy")]);
    assert_eq!(cell(&sets[0], 1, "address.city"), text("Rome"));
    run(s.as_mut(), "db.edits.drop()").await;
}
