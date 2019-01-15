use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    str,
    sync::{Arc, Mutex},
};

use failure::Fail;

use rbx_tree::{RbxTree, RbxInstance, RbxValue, RbxId};

use crate::{
    project::{Project, ProjectNode, InstanceProjectNodeMetadata},
    message_queue::MessageQueue,
    imfs::{Imfs, ImfsItem, ImfsFile, ImfsDirectory},
    path_map::PathMap,
    rbx_snapshot::{RbxSnapshotInstance, InstanceChanges, snapshot_from_tree, reify_root, reconcile_subtree},
};

const INIT_SCRIPT: &str = "init.lua";
const INIT_SERVER_SCRIPT: &str = "init.server.lua";
const INIT_CLIENT_SCRIPT: &str = "init.client.lua";

pub struct SyncPointMetadata {
    instance_name: String,
    children: HashMap<String, ProjectNode>,
}

pub struct RbxSession {
    tree: RbxTree,
    path_map: PathMap<RbxId>,
    instance_metadata_map: HashMap<RbxId, InstanceProjectNodeMetadata>,
    sync_point_metadata: HashMap<PathBuf, SyncPointMetadata>,
    message_queue: Arc<MessageQueue<InstanceChanges>>,
    imfs: Arc<Mutex<Imfs>>,
}

impl RbxSession {
    pub fn new(
        project: Arc<Project>,
        imfs: Arc<Mutex<Imfs>>,
        message_queue: Arc<MessageQueue<InstanceChanges>>,
    ) -> RbxSession {
        let mut sync_point_metadata = HashMap::new();
        let mut path_map = PathMap::new();
        let mut instance_metadata_map = HashMap::new();

        let tree = {
            let temp_imfs = imfs.lock().unwrap();
            construct_initial_tree(&project, &temp_imfs, &mut path_map, &mut instance_metadata_map, &mut sync_point_metadata)
        };

        RbxSession {
            tree,
            path_map,
            instance_metadata_map,
            sync_point_metadata,
            message_queue,
            imfs,
        }
    }

    fn path_created_or_updated(&mut self, path: &Path) {
        // TODO: Track paths actually updated in each step so we can ignore
        // redundant changes.
        let mut changes = InstanceChanges::default();

        {
            let imfs = self.imfs.lock().unwrap();
            let root_path = imfs.get_root_for_path(path)
                .expect("Path was outside in-memory filesystem roots");

            // Find the closest instance in the tree that currently exists
            let mut path_to_snapshot = self.path_map.descend(root_path, path);
            let &instance_id = self.path_map.get(&path_to_snapshot).unwrap();

            // If this is a file that might affect its parent if modified, we
            // should snapshot its parent instead.
            match path_to_snapshot.file_name().unwrap().to_str() {
                Some(INIT_SCRIPT) | Some(INIT_SERVER_SCRIPT) | Some(INIT_CLIENT_SCRIPT) => {
                    path_to_snapshot.pop();
                },
                _ => {},
            }

            trace!("Snapshotting path {}", path_to_snapshot.display());

            let maybe_snapshot = snapshot_instances_from_imfs(&imfs, &path_to_snapshot, &mut self.sync_point_metadata)
                .unwrap_or_else(|_| panic!("Could not generate instance snapshot for path {}", path_to_snapshot.display()));

            let snapshot = match maybe_snapshot {
                Some(snapshot) => snapshot,
                None => {
                    trace!("Path resulted in no snapshot being generated.");
                    return;
                },
            };

            trace!("Snapshot: {:#?}", snapshot);

            reconcile_subtree(
                &mut self.tree,
                instance_id,
                &snapshot,
                &mut self.path_map,
                &mut self.instance_metadata_map,
                &mut changes,
            );
        }

        if changes.is_empty() {
            trace!("No instance changes triggered from file update.");
        } else {
            trace!("Pushing changes: {}", changes);
            self.message_queue.push_messages(&[changes]);
        }
    }

    pub fn path_created(&mut self, path: &Path) {
        info!("Path created: {}", path.display());
        self.path_created_or_updated(path);
    }

    pub fn path_updated(&mut self, path: &Path) {
        info!("Path updated: {}", path.display());

        {
            let imfs = self.imfs.lock().unwrap();

            // If the path doesn't exist or is a directory, we don't care if it
            // updated
            match imfs.get(path) {
                Some(ImfsItem::Directory(_)) | None => {
                    trace!("Updated path was a directory, ignoring.");
                    return;
                },
                Some(ImfsItem::File(_)) => {},
            }
        }

        self.path_created_or_updated(path);
    }

    pub fn path_removed(&mut self, path: &Path) {
        info!("Path removed: {}", path.display());
        self.path_map.remove(path);
        self.path_created_or_updated(path);
    }

    pub fn path_renamed(&mut self, from_path: &Path, to_path: &Path) {
        info!("Path renamed from {} to {}", from_path.display(), to_path.display());
        self.path_map.remove(from_path);
        self.path_created_or_updated(from_path);
        self.path_created_or_updated(to_path);
    }

    pub fn get_tree(&self) -> &RbxTree {
        &self.tree
    }

    pub fn get_instance_metadata_map(&self) -> &HashMap<RbxId, InstanceProjectNodeMetadata> {
        &self.instance_metadata_map
    }

    pub fn debug_get_path_map(&self) -> &PathMap<RbxId> {
        &self.path_map
    }
}

pub fn construct_oneoff_tree(project: &Project, imfs: &Imfs) -> RbxTree {
    let mut path_map = PathMap::new();
    let mut instance_metadata_map = HashMap::new();
    let mut sync_point_metadata = HashMap::new();
    construct_initial_tree(project, imfs, &mut path_map, &mut instance_metadata_map, &mut sync_point_metadata)
}

fn construct_initial_tree(
    project: &Project,
    imfs: &Imfs,
    path_map: &mut PathMap<RbxId>,
    instance_metadata_map: &mut HashMap<RbxId, InstanceProjectNodeMetadata>,
    sync_point_metadata: &mut HashMap<PathBuf, SyncPointMetadata>,
) -> RbxTree {
    let snapshot = construct_project_node(
        imfs,
        Cow::Borrowed(&project.name),
        &project.tree,
        sync_point_metadata,
    );

    let mut changes = InstanceChanges::default();
    let tree = reify_root(&snapshot, path_map, instance_metadata_map, &mut changes);

    tree
}

fn construct_project_node<'a, 'b>(
    imfs: &'a Imfs,
    instance_name: Cow<'a, str>,
    project_node: &'b ProjectNode,
    sync_point_metadata: &'b mut HashMap<PathBuf, SyncPointMetadata>,
) -> RbxSnapshotInstance<'a> {
    match project_node {
        ProjectNode::Instance(node) => {
            let mut children = Vec::new();

            for (child_name, child_project_node) in &node.children {
                children.push(construct_project_node(imfs, Cow::Owned(child_name.to_owned()), child_project_node, sync_point_metadata));
            }

            RbxSnapshotInstance {
                class_name: Cow::Owned(node.class_name.clone()),
                name: instance_name,
                properties: node.properties.clone(),
                children,
                source_path: None,
                metadata: Some(node.metadata.clone()),
            }
        },
        ProjectNode::SyncPoint(node) => {
            // TODO: Propagate errors upward instead of dying
            let mut snapshot = snapshot_instances_from_imfs(imfs, &node.path, sync_point_metadata)
                .expect("Could not reify nodes from Imfs")
                .expect("Sync point node did not result in an instance");

            snapshot.name = instance_name.clone();
            sync_point_metadata.insert(node.path.clone(), SyncPointMetadata {
                instance_name: instance_name.to_string(),
                children: node.children.clone(),
            });

            snapshot
        },
    }
}

#[derive(Debug, Clone, Copy)]
enum FileType {
    ModuleScript,
    ServerScript,
    ClientScript,
    StringValue,
    LocalizationTable,
    XmlModel,
    BinaryModel,
}

fn get_trailing<'a>(input: &'a str, trailer: &str) -> Option<&'a str> {
    if input.ends_with(trailer) {
        let end = input.len().saturating_sub(trailer.len());
        Some(&input[..end])
    } else {
        None
    }
}

fn classify_file(file: &ImfsFile) -> Option<(&str, FileType)> {
    static EXTENSIONS_TO_TYPES: &[(&str, FileType)] = &[
        (".server.lua", FileType::ServerScript),
        (".client.lua", FileType::ClientScript),
        (".lua", FileType::ModuleScript),
        (".csv", FileType::LocalizationTable),
        (".txt", FileType::StringValue),
        (".rbxmx", FileType::XmlModel),
        (".rbxm", FileType::BinaryModel),
    ];

    let file_name = file.path.file_name()?.to_str()?;

    for (extension, file_type) in EXTENSIONS_TO_TYPES {
        if let Some(instance_name) = get_trailing(file_name, extension) {
            return Some((instance_name, *file_type))
        }
    }

    None
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LocalizationEntryCsv {
    key: String,
    context: String,
    example: String,
    source: String,
    #[serde(flatten)]
    values: HashMap<String, String>,
}

impl LocalizationEntryCsv {
    fn to_json(self) -> LocalizationEntryJson {
        LocalizationEntryJson {
            key: self.key,
            context: self.context,
            example: self.example,
            source: self.source,
            values: self.values,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LocalizationEntryJson {
    key: String,
    context: String,
    example: String,
    source: String,
    values: HashMap<String, String>,
}

#[derive(Debug, Fail)]
enum SnapshotError {
    DidNotExist(PathBuf),

    // TODO: Add file path to the error message?
    Utf8Error {
        #[fail(cause)]
        inner: str::Utf8Error,
        path: PathBuf,
    },

    XmlModelDecodeError {
        inner: rbx_xml::DecodeError,
        path: PathBuf,
    },

    BinaryModelDecodeError {
        inner: rbx_binary::DecodeError,
        path: PathBuf,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, output: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SnapshotError::DidNotExist(path) => write!(output, "Path did not exist: {}", path.display()),
            SnapshotError::Utf8Error { inner, path } => {
                write!(output, "Invalid UTF-8: {} in path {}", inner, path.display())
            },
            SnapshotError::XmlModelDecodeError { inner, path } => {
                write!(output, "Malformed rbxmx model: {:?} in path {}", inner, path.display())
            },
            SnapshotError::BinaryModelDecodeError { inner, path } => {
                write!(output, "Malformed rbxm model: {:?} in path {}", inner, path.display())
            },
        }
    }
}

fn snapshot_xml_model<'a>(
    instance_name: Cow<'a, str>,
    file: &ImfsFile,
) -> Result<Option<RbxSnapshotInstance<'a>>, SnapshotError> {
    let mut temp_tree = RbxTree::new(RbxInstance {
        name: "Temp".to_owned(),
        class_name: "Folder".to_owned(),
        properties: HashMap::new(),
    });

    let root_id = temp_tree.get_root_id();
    rbx_xml::decode(&mut temp_tree, root_id, file.contents.as_slice())
        .map_err(|inner| SnapshotError::XmlModelDecodeError {
            inner,
            path: file.path.clone(),
        })?;

    let root_instance = temp_tree.get_instance(root_id).unwrap();
    let children = root_instance.get_children_ids();

    match children.len() {
        0 => Ok(None),
        1 => {
            let mut snapshot = snapshot_from_tree(&temp_tree, children[0]).unwrap();
            snapshot.name = instance_name;
            Ok(Some(snapshot))
        },
        _ => panic!("Rojo doesn't have support for model files with multiple roots yet"),
    }
}

fn snapshot_binary_model<'a>(
    instance_name: Cow<'a, str>,
    file: &ImfsFile,
) -> Result<Option<RbxSnapshotInstance<'a>>, SnapshotError> {
    let mut temp_tree = RbxTree::new(RbxInstance {
        name: "Temp".to_owned(),
        class_name: "Folder".to_owned(),
        properties: HashMap::new(),
    });

    let root_id = temp_tree.get_root_id();
    rbx_binary::decode(&mut temp_tree, root_id, file.contents.as_slice())
        .map_err(|inner| SnapshotError::BinaryModelDecodeError {
            inner,
            path: file.path.clone(),
        })?;

    let root_instance = temp_tree.get_instance(root_id).unwrap();
    let children = root_instance.get_children_ids();

    match children.len() {
        0 => Ok(None),
        1 => {
            let mut snapshot = snapshot_from_tree(&temp_tree, children[0]).unwrap();
            snapshot.name = instance_name;
            Ok(Some(snapshot))
        },
        _ => panic!("Rojo doesn't have support for model files with multiple roots yet"),
    }
}

fn snapshot_instances_from_file<'a>(
    file: &'a ImfsFile,
    sync_point_metadata: &mut HashMap<PathBuf, SyncPointMetadata>,
) -> Result<Option<RbxSnapshotInstance<'a>>, SnapshotError> {
    let (instance_name, file_type) = match classify_file(file) {
        Some(info) => info,
        None => return Ok(None),
    };

    let instance_name = if let Some(metadata) = sync_point_metadata.get(&file.path) {
        Cow::Owned(metadata.instance_name.clone())
    } else {
        Cow::Borrowed(instance_name)
    };

    let class_name = match file_type {
        FileType::ModuleScript => "ModuleScript",
        FileType::ServerScript => "Script",
        FileType::ClientScript => "LocalScript",
        FileType::StringValue => "StringValue",
        FileType::LocalizationTable => "LocalizationTable",
        FileType::XmlModel => return snapshot_xml_model(instance_name, file),
        FileType::BinaryModel => return snapshot_binary_model(instance_name, file),
    };

    let contents = str::from_utf8(&file.contents)
        .map_err(|inner| SnapshotError::Utf8Error {
            inner,
            path: file.path.clone(),
        })?;

    let mut properties = HashMap::new();

    match file_type {
        FileType::ModuleScript | FileType::ServerScript | FileType::ClientScript => {
            properties.insert(String::from("Source"), RbxValue::String {
                value: contents.to_string(),
            });
        },
        FileType::StringValue => {
            properties.insert(String::from("Value"), RbxValue::String {
                value: contents.to_string(),
            });
        },
        FileType::LocalizationTable => {
            let entries: Vec<LocalizationEntryJson> = csv::Reader::from_reader(contents.as_bytes())
                .deserialize()
                .map(|result| result.expect("Malformed localization table found!"))
                .map(LocalizationEntryCsv::to_json)
                .collect();

            let table_contents = serde_json::to_string(&entries)
                .expect("Could not encode JSON for localization table");

            properties.insert(String::from("Contents"), RbxValue::String {
                value: table_contents,
            });
        },
        FileType::XmlModel | FileType::BinaryModel => unreachable!(),
    }

    Ok(Some(RbxSnapshotInstance {
        name: instance_name,
        class_name: Cow::Borrowed(class_name),
        properties,
        children: Vec::new(),
        source_path: Some(file.path.clone()),
        metadata: None,
    }))
}

fn snapshot_instances_from_directory<'a>(
    imfs: &'a Imfs,
    directory: &'a ImfsDirectory,
    sync_point_metadata: &mut HashMap<PathBuf, SyncPointMetadata>,
) -> Result<Option<RbxSnapshotInstance<'a>>, SnapshotError> {
    // TODO: Expand init support to handle server and client scripts
    let init_path = directory.path.join(INIT_SCRIPT);
    let init_server_path = directory.path.join(INIT_SERVER_SCRIPT);
    let init_client_path = directory.path.join(INIT_CLIENT_SCRIPT);

    let mut instance = if directory.children.contains(&init_path) {
        snapshot_instances_from_imfs(imfs, &init_path, sync_point_metadata)?
            .expect("Could not snapshot instance from file that existed!")
    } else if directory.children.contains(&init_server_path) {
        snapshot_instances_from_imfs(imfs, &init_server_path, sync_point_metadata)?
            .expect("Could not snapshot instance from file that existed!")
    } else if directory.children.contains(&init_client_path) {
        snapshot_instances_from_imfs(imfs, &init_client_path, sync_point_metadata)?
            .expect("Could not snapshot instance from file that existed!")
    } else {
        RbxSnapshotInstance {
            class_name: Cow::Borrowed("Folder"),
            name: Cow::Borrowed(""),
            properties: HashMap::new(),
            children: Vec::new(),
            source_path: Some(directory.path.clone()),
            metadata: None,
        }
    };

    for child_path in &directory.children {
        match child_path.file_name().unwrap().to_str().unwrap() {
            INIT_SCRIPT | INIT_SERVER_SCRIPT | INIT_CLIENT_SCRIPT => {
                // The existence of files with these names modifies the
                // parent instance and is handled above, so we can skip
                // them here.
            },
            _ => {
                match snapshot_instances_from_imfs(imfs, child_path, sync_point_metadata)? {
                    Some(child) => {
                        instance.children.push(child);
                    },
                    None => {},
                }
            },
        }
    }

    // We have to be careful not to lose instance names that are specified in
    // the project manifest. We store them in sync_point_metadata when the
    // original tree is constructed.
    instance.name = if let Some(metadata) = sync_point_metadata.get(&directory.path) {
        Cow::Owned(metadata.instance_name.clone())
    } else {
        Cow::Borrowed(directory.path
            .file_name().expect("Could not extract file name")
            .to_str().expect("Could not convert path to UTF-8"))
    };

    Ok(Some(instance))
}

fn snapshot_instances_from_imfs<'a, 'b>(
    imfs: &'a Imfs,
    imfs_path: &'b Path,
    sync_point_metadata: &'b mut HashMap<PathBuf, SyncPointMetadata>,
) -> Result<Option<RbxSnapshotInstance<'a>>, SnapshotError> {
    let instance = match imfs.get(imfs_path) {
        Some(ImfsItem::File(file)) => snapshot_instances_from_file(file, sync_point_metadata)?,
        Some(ImfsItem::Directory(directory)) => snapshot_instances_from_directory(imfs, directory, sync_point_metadata)?,
        None => return Err(SnapshotError::DidNotExist(imfs_path.to_path_buf())),
    };

    let mut instance = match instance {
        Some(instance) => instance,
        None => return Ok(None),
    };

    // Additional children might be specified for this in stance in the sync
    // point metadata!
    if let Some(metadata) = sync_point_metadata.get(imfs_path) {
        // sync_point_metadata might be mutated while we're iterating
        // over it, so let's clone our children out.
        let children = metadata.children.clone();

        for (child_name, child_node) in &children {
            let child = construct_project_node(imfs, Cow::Owned(child_name.to_owned()), child_node, sync_point_metadata);
            instance.children.push(child);
        }
    }

    Ok(Some(instance))
}