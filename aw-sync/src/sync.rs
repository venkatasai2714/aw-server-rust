/// Basic syncing for ActivityWatch
/// Based on: https://github.com/ActivityWatch/aw-server/pull/50
///
/// This does not handle any direct peer interaction/connections/networking, it works as a "bring your own folder synchronizer".
///
/// It manages a sync-folder by syncing the aw-server datastore with a copy/staging datastore in the folder (one for each host).
/// The sync folder is then synced with remotes using Syncthing/Dropbox/whatever.
extern crate chrono;
extern crate reqwest;
extern crate serde_json;

use std::fs;
use std::path::{Path, PathBuf};

use aw_client_rust::AwClient;
use chrono::{DateTime, Duration, Utc};

use aw_datastore::{Datastore, DatastoreError};
use aw_models::{Bucket, Event};

use crate::accessmethod::AccessMethod;

fn setup_local_remote(client: &AwClient, sync_directory: &Path) -> Result<Datastore, String> {
    // FIXME: Don't run twice if already exists
    fs::create_dir_all(sync_directory).unwrap();

    let info = client.get_info().unwrap();
    let remotedir = sync_directory.join(info.device_id.as_str());
    fs::create_dir_all(&remotedir).unwrap();

    let dbfile = remotedir
        .join("test.db")
        .into_os_string()
        .into_string()
        .unwrap();

    let ds_localremote = Datastore::new(dbfile, false);
    info!("Set up remote for local device");

    Ok(ds_localremote)
}

/// Performs a single sync pass
pub fn sync_run(
    sync_directory: &Path,
    client: AwClient,
    buckets: &Vec<String>,
    start: Option<DateTime<Utc>>,
) -> Result<(), String> {
    let ds_localremote = setup_local_remote(&client, sync_directory)?;

    //let ds_remotes = setup_test(sync_directory).unwrap();
    //info!("Set up remotes for testing");

    let info = client.get_info().unwrap();
    let remote_dbfiles = find_remotes_nonlocal(sync_directory, info.device_id.as_str());
    info!("Found remotes: {:?}", remote_dbfiles);

    // TODO: Check for compatible remote db version before opening
    let ds_remotes: Vec<Datastore> = remote_dbfiles.iter().map(create_datastore).collect();

    // Pull
    info!("Pulling...");
    for ds_from in &ds_remotes {
        sync_datastores(ds_from, &client, false, None, &buckets);
    }

    // Push local server buckets to sync folder
    info!("Pushing...");
    sync_datastores(
        &client,
        &ds_localremote,
        true,
        Some(info.device_id.as_str()),
        &buckets,
    );

    list_buckets(&client, sync_directory);

    Ok(())
}

pub fn list_buckets(client: &AwClient, sync_directory: &Path) {
    let ds_localremote = setup_local_remote(client, sync_directory).unwrap();

    let info = client.get_info().unwrap();
    let remote_dbfiles = find_remotes_nonlocal(sync_directory, info.device_id.as_str());
    info!("Found remotes: {:?}", remote_dbfiles);

    // TODO: Check for compatible remote db version before opening
    let ds_remotes: Vec<Datastore> = remote_dbfiles.iter().map(create_datastore).collect();

    log_buckets(client);
    log_buckets(&ds_localremote);
    for ds_from in &ds_remotes {
        log_buckets(ds_from);
    }
}

/// Returns a list of all remote dbs
fn find_remotes(sync_directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    println!("{}", sync_directory.display());
    let dbs = fs::read_dir(sync_directory)?
        .map(|res| res.ok().unwrap().path())
        .filter(|p| p.is_dir())
        .flat_map(|d| {
            //println!("{}", d.to_str().unwrap());
            fs::read_dir(d).unwrap()
        })
        .map(|res| res.ok().unwrap().path())
        .filter(|path| path.extension().unwrap() == "db") // FIXME: Is this the correct file ext?
        .collect();
    Ok(dbs)
}

/// Returns a list of all remotes, excluding local ones
fn find_remotes_nonlocal(sync_directory: &Path, device_id: &str) -> Vec<PathBuf> {
    let remotes_all = find_remotes(sync_directory).unwrap();
    // Filter out own remote
    remotes_all
        .into_iter()
        .filter(|path| {
            !path
                .clone()
                .into_os_string()
                .into_string()
                .unwrap()
                .contains(device_id)
        })
        .collect()
}

fn create_datastore(dspath: &PathBuf) -> Datastore {
    let pathstr = dspath.clone().into_os_string().into_string().unwrap();
    Datastore::new(pathstr, false)
}

fn setup_test(sync_directory: &Path) -> std::io::Result<Vec<Datastore>> {
    let mut datastores: Vec<Datastore> = Vec::new();
    for n in 0..2 {
        let dspath = sync_directory.join(format!("test-remote-{}.db", n));
        let ds_ = create_datastore(&dspath);
        let ds = &ds_ as &dyn AccessMethod;

        // Create a bucket
        // NOTE: Created with duplicate name to make sure it still works under such conditions
        let bucket_jsonstr = format!(
            r#"{{
                "id": "bucket",
                "type": "test",
                "hostname": "device-{}",
                "client": "test"
            }}"#,
            n
        );
        let bucket: Bucket = serde_json::from_str(&bucket_jsonstr)?;
        match ds.create_bucket(&bucket) {
            Ok(()) => (),
            Err(e) => match e {
                DatastoreError::BucketAlreadyExists(_) => {
                    debug!("bucket already exists, skipping");
                }
                e => panic!("woops! {:?}", e),
            },
        };

        // Insert some testing events into the bucket
        let events: Vec<Event> = (0..3)
            .map(|i| {
                let timestamp: DateTime<Utc> = Utc::now() + Duration::milliseconds(i * 10);
                let event_jsonstr = format!(
                    r#"{{
                "timestamp": "{}",
                "duration": 0,
                "data": {{"test": {} }}
            }}"#,
                    timestamp.to_rfc3339(),
                    i
                );
                serde_json::from_str(&event_jsonstr).unwrap()
            })
            .collect::<Vec<Event>>();

        ds.insert_events(bucket.id.as_str(), events).unwrap();
        //let new_eventcount = ds.get_event_count(bucket.id.as_str(), None, None).unwrap();
        //info!("Eventcount: {:?} ({} new)", new_eventcount, events.len());
        datastores.push(ds_);
    }
    Ok(datastores)
}

/// Returns the sync-destination bucket for a given bucket, creates it if it doesn't exist.
fn get_or_create_sync_bucket(
    bucket_from: &Bucket,
    ds_to: &dyn AccessMethod,
    is_push: bool,
) -> Bucket {
    let new_id = if is_push {
        bucket_from.id.clone()
    } else {
        // Ensure the bucket ID ends in "-synced-from-{device id}"
        let orig_bucketid = bucket_from.id.split("-synced-from-").next().unwrap();
        let fallback = serde_json::to_value(&bucket_from.hostname).unwrap();
        let origin = bucket_from
            .data
            .get("$aw.sync.origin")
            .unwrap_or(&fallback)
            .as_str()
            .unwrap();
        format!("{}-synced-from-{}", orig_bucketid, origin)
    };

    match ds_to.get_bucket(new_id.as_str()) {
        Ok(bucket) => bucket,
        Err(DatastoreError::NoSuchBucket(_)) => {
            let mut bucket_new = bucket_from.clone();
            bucket_new.id = new_id.clone();
            // TODO: Replace sync origin with hostname/GUID and discuss how we will treat the data
            // attributes for internal use.
            bucket_new.data.insert(
                "$aw.sync.origin".to_string(),
                serde_json::json!(bucket_from.hostname),
            );
            ds_to.create_bucket(&bucket_new).unwrap();
            ds_to.get_bucket(new_id.as_str()).unwrap()
        }
        Err(e) => panic!("{:?}", e),
    }
}

/// Syncs all buckets from `ds_from` to `ds_to` with `-synced` appended to the ID of the destination bucket.
///
/// is_push: a bool indicating if we're pushing local buckets to the sync dir
///          (as opposed to pulling from remotes)
/// src_did: source device ID
pub fn sync_datastores(
    ds_from: &dyn AccessMethod,
    ds_to: &dyn AccessMethod,
    is_push: bool,
    src_did: Option<&str>,
    buckets: &Vec<String>,
) {
    // FIXME: "-synced" should only be appended when synced to the local database, not to the
    // staging area for local buckets.
    info!("Syncing {:?} to {:?}", ds_from, ds_to);

    let mut buckets_from: Vec<Bucket> = ds_from
        .get_buckets()
        .unwrap()
        .iter_mut()
        .map(|tup| {
            // TODO: Refuse to sync buckets without hostname/device ID set, or if set to 'unknown'
            if tup.1.hostname == "unknown" {
                warn!("Bucket hostname/device ID was invalid, setting to device ID/hostname");
                tup.1.hostname = src_did.unwrap().to_string();
            }
            tup.1.clone()
        })
        // Filter out buckets not in the buckets vec
        .filter(|bucket| buckets.iter().any(|b_id| b_id == &bucket.id))
        .collect();

    // Sync buckets in order of most recently updated
    buckets_from.sort_by_key(|b| b.metadata.end);

    for bucket_from in buckets_from {
        let bucket_to = get_or_create_sync_bucket(&bucket_from, ds_to, is_push);
        sync_one(ds_from, ds_to, bucket_from, bucket_to);
    }
}

/// Syncs a single bucket from one datastore to another
fn sync_one(
    ds_from: &dyn AccessMethod,
    ds_to: &dyn AccessMethod,
    bucket_from: Bucket,
    bucket_to: Bucket,
) {
    let eventcount_to_old = ds_to.get_event_count(bucket_to.id.as_str()).unwrap();
    info!("Bucket: {:?}", bucket_to.id);

    // Sync events
    // FIXME: This should use bucket_to.metadata.end, but it doesn't because it doesn't work
    // for empty buckets (Should be None, is Some(unknown_time))
    // let resume_sync_at = bucket_to.metadata.end;
    let most_recent_events = ds_to
        .get_events(bucket_to.id.as_str(), None, None, Some(1))
        .unwrap();
    let resume_sync_at = most_recent_events.first().map(|e| e.timestamp + e.duration);

    info!("Resumed at: {:?}", resume_sync_at);
    let mut events: Vec<Event> = ds_from
        .get_events(bucket_from.id.as_str(), resume_sync_at, None, None)
        .unwrap()
        .iter()
        .map(|e| {
            let mut new_e = e.clone();
            new_e.id = None;
            new_e
        })
        .collect();

    // Sort ascending
    events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    //info!("{:?}", events);

    // TODO: Do bulk insert using insert_events instead? (for performance)
    for event in events {
        print!("\r{}", event.timestamp);
        ds_to.heartbeat(bucket_to.id.as_str(), event, 0.0).unwrap();
    }

    let eventcount_to_new = ds_to.get_event_count(bucket_to.id.as_str()).unwrap();
    info!(
        "Synced {} new events",
        eventcount_to_new - eventcount_to_old
    );
}

fn log_buckets(ds: &dyn AccessMethod) {
    // Logs all buckets and some metadata for a given datastore
    let buckets = ds.get_buckets().unwrap();
    info!("Buckets in {:?}:", ds);
    for bucket in buckets.values() {
        info!(" - {}", bucket.id.as_str());
        info!(
            "   eventcount: {:?}",
            ds.get_event_count(bucket.id.as_str()).unwrap()
        );
    }
}
