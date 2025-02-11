// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use crate::builtin::{BuiltinLog, BUILTIN_CLUSTERS, BUILTIN_CLUSTER_REPLICAS};
use crate::objects::{
    AuditLogKey, Cluster, ClusterIntrospectionSourceIndexKey, ClusterIntrospectionSourceIndexValue,
    ClusterKey, ClusterReplica, ClusterReplicaKey, ClusterReplicaValue, ClusterValue, CommentKey,
    CommentValue, ConfigKey, ConfigValue, Database, DatabaseKey, DatabaseValue,
    DefaultPrivilegesKey, DefaultPrivilegesValue, GidMappingKey, GidMappingValue, IdAllocKey,
    IdAllocValue, Item, ItemKey, ItemValue, ReplicaConfig, Role, RoleKey, RoleValue, Schema,
    SchemaKey, SchemaValue, ServerConfigurationKey, ServerConfigurationValue, SettingKey,
    SettingValue, StorageUsageKey, SystemObjectMapping, SystemPrivilegesKey, SystemPrivilegesValue,
    TimestampKey, TimestampValue,
};
use crate::objects::{ClusterConfig, ClusterVariant};
use crate::{
    BootstrapArgs, DurableCatalogState, Error, ReplicaLocation, DATABASE_ID_ALLOC_KEY,
    SCHEMA_ID_ALLOC_KEY, SYSTEM_CLUSTER_ID_ALLOC_KEY, SYSTEM_REPLICA_ID_ALLOC_KEY,
    USER_ROLE_ID_ALLOC_KEY,
};
use itertools::Itertools;
use mz_audit_log::{VersionedEvent, VersionedStorageUsage};
use mz_controller::clusters::ReplicaLogging;
use mz_controller_types::{ClusterId, ReplicaId};
use mz_proto::RustType;
use mz_repr::adt::mz_acl_item::{AclMode, MzAclItem};
use mz_repr::role_id::RoleId;
use mz_repr::{Diff, GlobalId};
use mz_sql::catalog::{
    CatalogError as SqlCatalogError, ObjectType, RoleAttributes, RoleMembership, RoleVars,
};
use mz_sql::names::{
    CommentObjectId, DatabaseId, ItemQualifiers, QualifiedItemName, ResolvedDatabaseSpecifier,
    SchemaId, SchemaSpecifier,
};
use mz_sql::session::user::MZ_SYSTEM_ROLE_ID;
use mz_sql_parser::ast::QualifiedReplica;
use mz_stash::objects::proto;
use mz_stash::TableTransaction;
use mz_storage_types::sources::Timeline;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub(crate) fn add_new_builtin_clusters_migration(txn: &mut Transaction<'_>) -> Result<(), Error> {
    let cluster_names: BTreeSet<_> = txn
        .clusters
        .items()
        .into_values()
        .map(|value| value.name)
        .collect();

    for builtin_cluster in BUILTIN_CLUSTERS {
        if !cluster_names.contains(builtin_cluster.name) {
            let id = txn.get_and_increment_id(SYSTEM_CLUSTER_ID_ALLOC_KEY.to_string())?;
            let id = ClusterId::System(id);
            txn.insert_system_cluster(
                id,
                builtin_cluster.name,
                vec![],
                builtin_cluster.privileges.to_vec(),
                ClusterConfig {
                    // TODO: Should builtin clusters be managed or unmanaged?
                    variant: ClusterVariant::Unmanaged,
                },
            )?;
        }
    }
    Ok(())
}

pub(crate) fn add_new_builtin_cluster_replicas_migration(
    txn: &mut Transaction<'_>,
    bootstrap_args: &BootstrapArgs,
) -> Result<(), Error> {
    let cluster_lookup: BTreeMap<_, _> = txn
        .clusters
        .items()
        .into_iter()
        .map(|(key, value)| (value.name, key.id))
        .collect();

    let replicas: BTreeMap<_, _> =
        txn.cluster_replicas
            .items()
            .into_values()
            .fold(BTreeMap::new(), |mut acc, value| {
                acc.entry(value.cluster_id)
                    .or_insert_with(BTreeSet::new)
                    .insert(value.name);
                acc
            });

    for builtin_replica in BUILTIN_CLUSTER_REPLICAS {
        let cluster_id = cluster_lookup
            .get(builtin_replica.cluster_name)
            .expect("builtin cluster replica references non-existent cluster");

        let replica_names = replicas.get(cluster_id);
        if matches!(replica_names, None)
            || matches!(replica_names, Some(names) if !names.contains(builtin_replica.name))
        {
            let replica_id = txn.get_and_increment_id(SYSTEM_REPLICA_ID_ALLOC_KEY.to_string())?;
            let replica_id = ReplicaId::System(replica_id);
            let config = builtin_cluster_replica_config(bootstrap_args);
            txn.insert_cluster_replica(
                *cluster_id,
                replica_id,
                builtin_replica.name,
                &config,
                MZ_SYSTEM_ROLE_ID,
            )?;
        }
    }
    Ok(())
}

pub(crate) fn builtin_cluster_replica_config(bootstrap_args: &BootstrapArgs) -> ReplicaConfig {
    ReplicaConfig {
        location: ReplicaLocation::Managed {
            size: bootstrap_args.builtin_cluster_replica_size.clone(),
            availability_zone: None,
            disk: false,
        },
        logging: default_logging_config(),
        idle_arrangement_merge_effort: None,
    }
}

fn default_logging_config() -> ReplicaLogging {
    ReplicaLogging {
        log_logging: false,
        interval: Some(Duration::from_secs(1)),
    }
}

/// A [`Transaction`] batches multiple [`crate::stash::Connection`] operations together and commits
/// them atomically.
pub struct Transaction<'a> {
    durable_catalog: &'a mut dyn DurableCatalogState,
    databases: TableTransaction<DatabaseKey, DatabaseValue>,
    schemas: TableTransaction<SchemaKey, SchemaValue>,
    items: TableTransaction<ItemKey, ItemValue>,
    comments: TableTransaction<CommentKey, CommentValue>,
    roles: TableTransaction<RoleKey, RoleValue>,
    clusters: TableTransaction<ClusterKey, ClusterValue>,
    cluster_replicas: TableTransaction<ClusterReplicaKey, ClusterReplicaValue>,
    introspection_sources:
        TableTransaction<ClusterIntrospectionSourceIndexKey, ClusterIntrospectionSourceIndexValue>,
    id_allocator: TableTransaction<IdAllocKey, IdAllocValue>,
    configs: TableTransaction<ConfigKey, ConfigValue>,
    settings: TableTransaction<SettingKey, SettingValue>,
    timestamps: TableTransaction<TimestampKey, TimestampValue>,
    system_gid_mapping: TableTransaction<GidMappingKey, GidMappingValue>,
    system_configurations: TableTransaction<ServerConfigurationKey, ServerConfigurationValue>,
    default_privileges: TableTransaction<DefaultPrivilegesKey, DefaultPrivilegesValue>,
    system_privileges: TableTransaction<SystemPrivilegesKey, SystemPrivilegesValue>,
    // Don't make this a table transaction so that it's not read into the stash
    // memory cache.
    audit_log_updates: Vec<(proto::AuditLogKey, (), i64)>,
    storage_usage_updates: Vec<(proto::StorageUsageKey, (), i64)>,
}

impl<'a> Transaction<'a> {
    pub fn new(
        durable_catalog: &'a mut dyn DurableCatalogState,
        databases: BTreeMap<proto::DatabaseKey, proto::DatabaseValue>,
        schemas: BTreeMap<proto::SchemaKey, proto::SchemaValue>,
        roles: BTreeMap<proto::RoleKey, proto::RoleValue>,
        items: BTreeMap<proto::ItemKey, proto::ItemValue>,
        comments: BTreeMap<proto::CommentKey, proto::CommentValue>,
        clusters: BTreeMap<proto::ClusterKey, proto::ClusterValue>,
        cluster_replicas: BTreeMap<proto::ClusterReplicaKey, proto::ClusterReplicaValue>,
        introspection_sources: BTreeMap<
            proto::ClusterIntrospectionSourceIndexKey,
            proto::ClusterIntrospectionSourceIndexValue,
        >,
        id_allocator: BTreeMap<proto::IdAllocKey, proto::IdAllocValue>,
        configs: BTreeMap<proto::ConfigKey, proto::ConfigValue>,
        settings: BTreeMap<proto::SettingKey, proto::SettingValue>,
        timestamps: BTreeMap<proto::TimestampKey, proto::TimestampValue>,
        system_gid_mapping: BTreeMap<proto::GidMappingKey, proto::GidMappingValue>,
        system_configurations: BTreeMap<
            proto::ServerConfigurationKey,
            proto::ServerConfigurationValue,
        >,
        default_privileges: BTreeMap<proto::DefaultPrivilegesKey, proto::DefaultPrivilegesValue>,
        system_privileges: BTreeMap<proto::SystemPrivilegesKey, proto::SystemPrivilegesValue>,
    ) -> Result<Transaction, Error> {
        Ok(Transaction {
            durable_catalog,
            databases: TableTransaction::new(databases, |a: &DatabaseValue, b| a.name == b.name)?,
            schemas: TableTransaction::new(schemas, |a: &SchemaValue, b| {
                a.database_id == b.database_id && a.name == b.name
            })?,
            items: TableTransaction::new(items, |a: &ItemValue, b| {
                a.schema_id == b.schema_id && a.name == b.name
            })?,
            comments: TableTransaction::new(comments, |_a, _b| false)?,
            roles: TableTransaction::new(roles, |a: &RoleValue, b| a.name == b.name)?,
            clusters: TableTransaction::new(clusters, |a: &ClusterValue, b| a.name == b.name)?,
            cluster_replicas: TableTransaction::new(
                cluster_replicas,
                |a: &ClusterReplicaValue, b| a.cluster_id == b.cluster_id && a.name == b.name,
            )?,
            introspection_sources: TableTransaction::new(introspection_sources, |_a, _b| false)?,
            id_allocator: TableTransaction::new(id_allocator, |_a, _b| false)?,
            configs: TableTransaction::new(configs, |_a, _b| false)?,
            settings: TableTransaction::new(settings, |_a, _b| false)?,
            timestamps: TableTransaction::new(timestamps, |_a, _b| false)?,
            system_gid_mapping: TableTransaction::new(system_gid_mapping, |_a, _b| false)?,
            system_configurations: TableTransaction::new(system_configurations, |_a, _b| false)?,
            default_privileges: TableTransaction::new(default_privileges, |_a, _b| false)?,
            system_privileges: TableTransaction::new(system_privileges, |_a, _b| false)?,
            audit_log_updates: Vec::new(),
            storage_usage_updates: Vec::new(),
        })
    }

    pub fn loaded_items(&self) -> Vec<Item> {
        let databases = self.databases.items();
        let schemas = self.schemas.items();
        let mut items = Vec::new();
        self.items.for_values(|k, v| {
            let schema_key = SchemaKey { id: v.schema_id };
            let schema = match schemas.get(&schema_key) {
                Some(schema) => schema,
                None => panic!(
                    "corrupt stash! unknown schema id {}, for item with key \
                        {k:?} and value {v:?}",
                    v.schema_id
                ),
            };
            let database_spec = match schema.database_id {
                Some(id) => {
                    let key = DatabaseKey { id };
                    if databases.get(&key).is_none() {
                        panic!(
                            "corrupt stash! unknown database id {key:?}, for item with key \
                        {k:?} and value {v:?}"
                        );
                    }
                    ResolvedDatabaseSpecifier::from(id)
                }
                None => ResolvedDatabaseSpecifier::Ambient,
            };
            items.push(Item {
                id: k.gid,
                name: QualifiedItemName {
                    qualifiers: ItemQualifiers {
                        database_spec,
                        schema_spec: SchemaSpecifier::from(v.schema_id),
                    },
                    item: v.name.clone(),
                },
                create_sql: v.create_sql.clone(),
                owner_id: v.owner_id,
                privileges: v.privileges.clone(),
            });
        });
        items.sort_by_key(|Item { id, .. }| *id);
        items
    }

    pub fn insert_audit_log_event(&mut self, event: VersionedEvent) {
        self.audit_log_updates
            .push((AuditLogKey { event }.into_proto(), (), 1));
    }

    pub fn insert_storage_usage_event(&mut self, metric: VersionedStorageUsage) {
        self.storage_usage_updates
            .push((StorageUsageKey { metric }.into_proto(), (), 1));
    }

    pub fn insert_user_database(
        &mut self,
        database_name: &str,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
    ) -> Result<DatabaseId, Error> {
        let id = self.get_and_increment_id(DATABASE_ID_ALLOC_KEY.to_string())?;
        match self.databases.insert(
            DatabaseKey {
                // TODO(parkertimmerman): Support creating databases in the System namespace.
                id: DatabaseId::User(id),
            },
            DatabaseValue {
                name: database_name.to_string(),
                owner_id,
                privileges,
            },
        ) {
            // TODO(parkertimmerman): Support creating databases in the System namespace.
            Ok(_) => Ok(DatabaseId::User(id)),
            Err(_) => Err(SqlCatalogError::DatabaseAlreadyExists(database_name.to_owned()).into()),
        }
    }

    pub fn insert_user_schema(
        &mut self,
        database_id: DatabaseId,
        schema_name: &str,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
    ) -> Result<SchemaId, Error> {
        let id = self.get_and_increment_id(SCHEMA_ID_ALLOC_KEY.to_string())?;
        match self.schemas.insert(
            SchemaKey {
                // TODO(parkertimmerman): Support creating schemas in the System namespace.
                id: SchemaId::User(id),
            },
            SchemaValue {
                database_id: Some(database_id),
                name: schema_name.to_string(),
                owner_id,
                privileges,
            },
        ) {
            // TODO(parkertimmerman): Support creating schemas in the System namespace.
            Ok(_) => Ok(SchemaId::User(id)),
            Err(_) => Err(SqlCatalogError::SchemaAlreadyExists(schema_name.to_owned()).into()),
        }
    }

    pub fn insert_user_role(
        &mut self,
        name: String,
        attributes: RoleAttributes,
        membership: RoleMembership,
        vars: RoleVars,
    ) -> Result<RoleId, Error> {
        let id = self.get_and_increment_id(USER_ROLE_ID_ALLOC_KEY.to_string())?;
        let id = RoleId::User(id);
        match self.roles.insert(
            RoleKey { id },
            RoleValue {
                name: name.clone(),
                attributes,
                membership,
                vars,
            },
        ) {
            Ok(_) => Ok(id),
            Err(_) => Err(SqlCatalogError::RoleAlreadyExists(name).into()),
        }
    }

    /// Panics if any introspection source id is not a system id
    pub fn insert_user_cluster(
        &mut self,
        cluster_id: ClusterId,
        cluster_name: &str,
        linked_object_id: Option<GlobalId>,
        introspection_source_indexes: Vec<(&'static BuiltinLog, GlobalId)>,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
        config: ClusterConfig,
    ) -> Result<(), Error> {
        self.insert_cluster(
            cluster_id,
            cluster_name,
            linked_object_id,
            introspection_source_indexes,
            owner_id,
            privileges,
            config,
        )
    }

    /// Panics if any introspection source id is not a system id
    fn insert_system_cluster(
        &mut self,
        cluster_id: ClusterId,
        cluster_name: &str,
        introspection_source_indexes: Vec<(&'static BuiltinLog, GlobalId)>,
        privileges: Vec<MzAclItem>,
        config: ClusterConfig,
    ) -> Result<(), Error> {
        self.insert_cluster(
            cluster_id,
            cluster_name,
            None,
            introspection_source_indexes,
            MZ_SYSTEM_ROLE_ID,
            privileges,
            config,
        )
    }

    fn insert_cluster(
        &mut self,
        cluster_id: ClusterId,
        cluster_name: &str,
        linked_object_id: Option<GlobalId>,
        introspection_source_indexes: Vec<(&'static BuiltinLog, GlobalId)>,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
        config: ClusterConfig,
    ) -> Result<(), Error> {
        if let Err(_) = self.clusters.insert(
            ClusterKey { id: cluster_id },
            ClusterValue {
                name: cluster_name.to_string(),
                linked_object_id,
                owner_id,
                privileges,
                config,
            },
        ) {
            return Err(SqlCatalogError::ClusterAlreadyExists(cluster_name.to_owned()).into());
        };

        for (builtin, index_id) in introspection_source_indexes {
            let index_id = if let GlobalId::System(id) = index_id {
                id
            } else {
                panic!("non-system id provided")
            };
            self.introspection_sources
                .insert(
                    ClusterIntrospectionSourceIndexKey {
                        cluster_id,
                        name: builtin.name.to_string(),
                    },
                    ClusterIntrospectionSourceIndexValue { index_id },
                )
                .expect("no uniqueness violation");
        }

        Ok(())
    }

    pub fn rename_cluster(
        &mut self,
        cluster_id: ClusterId,
        cluster_name: &str,
        cluster_to_name: &str,
    ) -> Result<(), Error> {
        let key = ClusterKey { id: cluster_id };

        match self.clusters.update(|k, v| {
            if *k == key {
                let mut value = v.clone();
                value.name = cluster_to_name.to_string();
                Some(value)
            } else {
                None
            }
        })? {
            0 => Err(SqlCatalogError::UnknownCluster(cluster_name.to_string()).into()),
            1 => Ok(()),
            n => panic!(
                "Expected to update single cluster {cluster_name} ({cluster_id}), updated {n}"
            ),
        }
    }

    pub fn check_migration_has_run(&mut self, name: String) -> Result<bool, Error> {
        let key = SettingKey { name };
        // If the key does not exist, then the migration has not been run.
        let has_run = self.settings.get(&key).as_ref().is_some();

        Ok(has_run)
    }

    pub fn mark_migration_has_run(&mut self, name: String) -> Result<(), Error> {
        let key = SettingKey { name };
        let val = SettingValue {
            value: true.to_string(),
        };
        self.settings.insert(key, val)?;

        Ok(())
    }

    pub fn rename_cluster_replica(
        &mut self,
        replica_id: ReplicaId,
        replica_name: &QualifiedReplica,
        replica_to_name: &str,
    ) -> Result<(), Error> {
        let key = ClusterReplicaKey { id: replica_id };

        match self.cluster_replicas.update(|k, v| {
            if *k == key {
                let mut value = v.clone();
                value.name = replica_to_name.to_string();
                Some(value)
            } else {
                None
            }
        })? {
            0 => Err(SqlCatalogError::UnknownClusterReplica(replica_name.to_string()).into()),
            1 => Ok(()),
            n => panic!(
                "Expected to update single cluster replica {replica_name} ({replica_id}), updated {n}"
            ),
        }
    }

    pub fn insert_cluster_replica(
        &mut self,
        cluster_id: ClusterId,
        replica_id: ReplicaId,
        replica_name: &str,
        config: &ReplicaConfig,
        owner_id: RoleId,
    ) -> Result<(), Error> {
        if let Err(_) = self.cluster_replicas.insert(
            ClusterReplicaKey { id: replica_id },
            ClusterReplicaValue {
                cluster_id,
                name: replica_name.into(),
                config: config.clone(),
                owner_id,
            },
        ) {
            let cluster = self
                .clusters
                .get(&ClusterKey { id: cluster_id })
                .expect("cluster exists");
            return Err(SqlCatalogError::DuplicateReplica(
                replica_name.to_string(),
                cluster.name.to_string(),
            )
            .into());
        };
        Ok(())
    }

    /// Updates persisted information about persisted introspection source
    /// indexes.
    ///
    /// Panics if provided id is not a system id.
    pub fn update_introspection_source_index_gids(
        &mut self,
        mappings: impl Iterator<Item = (ClusterId, impl Iterator<Item = (String, GlobalId)>)>,
    ) -> Result<(), Error> {
        for (cluster_id, updates) in mappings {
            for (name, id) in updates {
                let index_id = if let GlobalId::System(index_id) = id {
                    index_id
                } else {
                    panic!("Introspection source index should have a system id")
                };
                let prev = self.introspection_sources.set(
                    ClusterIntrospectionSourceIndexKey { cluster_id, name },
                    Some(ClusterIntrospectionSourceIndexValue { index_id }),
                )?;
                if prev.is_none() {
                    return Err(
                        SqlCatalogError::FailedBuiltinSchemaMigration(format!("{id}")).into(),
                    );
                }
            }
        }
        Ok(())
    }

    pub fn insert_item(
        &mut self,
        id: GlobalId,
        schema_id: SchemaId,
        item_name: &str,
        create_sql: String,
        owner_id: RoleId,
        privileges: Vec<MzAclItem>,
    ) -> Result<(), Error> {
        match self.items.insert(
            ItemKey { gid: id },
            ItemValue {
                schema_id,
                name: item_name.to_string(),
                create_sql,
                owner_id,
                privileges,
            },
        ) {
            Ok(_) => Ok(()),
            Err(_) => Err(SqlCatalogError::ItemAlreadyExists(id, item_name.to_owned()).into()),
        }
    }

    pub fn get_and_increment_id(&mut self, key: String) -> Result<u64, Error> {
        let id = self
            .id_allocator
            .items()
            .get(&IdAllocKey { name: key.clone() })
            .unwrap_or_else(|| panic!("{key} id allocator missing"))
            .next_id;
        let next_id = id.checked_add(1).ok_or(SqlCatalogError::IdExhaustion)?;
        let prev = self
            .id_allocator
            .set(IdAllocKey { name: key }, Some(IdAllocValue { next_id }))?;
        assert!(prev.is_some());
        Ok(id)
    }

    pub fn remove_database(&mut self, id: &DatabaseId) -> Result<(), Error> {
        let prev = self.databases.set(DatabaseKey { id: *id }, None)?;
        if prev.is_some() {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownDatabase(id.to_string()).into())
        }
    }

    pub fn remove_schema(
        &mut self,
        database_id: &Option<DatabaseId>,
        schema_id: &SchemaId,
    ) -> Result<(), Error> {
        let prev = self.schemas.set(SchemaKey { id: *schema_id }, None)?;
        if prev.is_some() {
            Ok(())
        } else {
            let database_name = match database_id {
                Some(id) => format!("{id}."),
                None => "".to_string(),
            };
            Err(SqlCatalogError::UnknownSchema(format!("{}.{}", database_name, schema_id)).into())
        }
    }

    pub fn remove_role(&mut self, name: &str) -> Result<(), Error> {
        let roles = self.roles.delete(|_k, v| v.name == name);
        assert!(
            roles.iter().all(|(k, _)| k.id.is_user()),
            "cannot delete non-user roles"
        );
        let n = roles.len();
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownRole(name.to_owned()).into())
        }
    }

    pub fn remove_cluster(&mut self, id: ClusterId) -> Result<(), Error> {
        let deleted = self.clusters.delete(|k, _v| k.id == id);
        if deleted.is_empty() {
            Err(SqlCatalogError::UnknownCluster(id.to_string()).into())
        } else {
            assert_eq!(deleted.len(), 1);
            // Cascade delete introspection sources and cluster replicas.
            //
            // TODO(benesch): this doesn't seem right. Cascade deletions should
            // be entirely the domain of the higher catalog layer, not the
            // storage layer.
            self.cluster_replicas.delete(|_k, v| v.cluster_id == id);
            self.introspection_sources
                .delete(|k, _v| k.cluster_id == id);
            Ok(())
        }
    }

    pub fn remove_cluster_replica(&mut self, id: ReplicaId) -> Result<(), Error> {
        let deleted = self.cluster_replicas.delete(|k, _v| k.id == id);
        if deleted.len() == 1 {
            Ok(())
        } else {
            assert!(deleted.is_empty());
            Err(SqlCatalogError::UnknownClusterReplica(id.to_string()).into())
        }
    }

    /// Removes item `id` from the transaction.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of items in the stash.
    /// DO NOT call this function in a loop, use [`Self::remove_items`] instead.
    pub fn remove_item(&mut self, id: GlobalId) -> Result<(), Error> {
        let prev = self.items.set(ItemKey { gid: id }, None)?;
        if prev.is_some() {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownItem(id.to_string()).into())
        }
    }

    /// Removes all items in `ids` from the transaction.
    ///
    /// Returns an error if any id in `ids` is not found.
    ///
    /// NOTE: On error, there still may be some items removed from the transaction. It is
    /// up to the called to either abort the transaction or commit.
    pub fn remove_items(&mut self, ids: BTreeSet<GlobalId>) -> Result<(), Error> {
        let n = self.items.delete(|k, _v| ids.contains(&k.gid)).len();
        if n == ids.len() {
            Ok(())
        } else {
            let item_gids = self.items.items().keys().map(|k| k.gid).collect();
            let mut unknown = ids.difference(&item_gids);
            Err(SqlCatalogError::UnknownItem(unknown.join(", ")).into())
        }
    }

    /// Updates item `id` in the transaction to `item_name` and `item`.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of items in the stash.
    /// DO NOT call this function in a loop, use [`Self::update_items`] instead.
    pub fn update_item(&mut self, id: GlobalId, item: Item) -> Result<(), Error> {
        let n = self.items.update(|k, v| {
            if k.gid == id {
                let item = item.clone();
                // Schema IDs cannot change.
                assert_eq!(
                    SchemaId::from(item.name.qualifiers.schema_spec),
                    v.schema_id
                );
                Some(ItemValue {
                    schema_id: v.schema_id,
                    name: item.name.item,
                    create_sql: item.create_sql,
                    owner_id: item.owner_id,
                    privileges: item.privileges,
                })
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownItem(id.to_string()).into())
        }
    }

    /// Updates all items with ids matching the keys of `items` in the transaction, to the
    /// corresponding value in `items`.
    ///
    /// Returns an error if any id in `items` is not found.
    ///
    /// NOTE: On error, there still may be some items updated in the transaction. It is
    /// up to the called to either abort the transaction or commit.
    pub fn update_items(&mut self, items: BTreeMap<GlobalId, Item>) -> Result<(), Error> {
        let n = self.items.update(|k, v| {
            if let Some(item) = items.get(&k.gid) {
                // Schema IDs cannot change.
                assert_eq!(
                    SchemaId::from(item.name.qualifiers.schema_spec),
                    v.schema_id
                );
                Some(ItemValue {
                    schema_id: v.schema_id,
                    name: item.name.item.clone(),
                    create_sql: item.create_sql.clone(),
                    owner_id: item.owner_id.clone(),
                    privileges: item.privileges.clone(),
                })
            } else {
                None
            }
        })?;
        let n = usize::try_from(n).expect("Must be positive and fit in usize");
        if n == items.len() {
            Ok(())
        } else {
            let update_ids: BTreeSet<_> = items.into_keys().collect();
            let item_ids: BTreeSet<_> = self.items.items().keys().map(|k| k.gid).collect();
            let mut unknown = update_ids.difference(&item_ids);
            Err(SqlCatalogError::UnknownItem(unknown.join(", ")).into())
        }
    }

    /// Updates role `id` in the transaction to `role`.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of items in the stash.
    /// DO NOT call this function in a loop, implement and use some `Self::update_roles` instead.
    /// You should model it after [`Self::update_items`].
    pub fn update_role(&mut self, id: RoleId, role: Role) -> Result<(), Error> {
        let n = self.roles.update(move |k, _v| {
            if k.id == id {
                let role = role.clone();
                Some(RoleValue::from(role))
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownItem(id.to_string()).into())
        }
    }

    /// Updates persisted mapping from system objects to global IDs and fingerprints. Each element
    /// of `mappings` should be (old-global-id, new-system-object-mapping).
    ///
    /// Panics if provided id is not a system id.
    pub fn update_system_object_mappings(
        &mut self,
        mappings: BTreeMap<GlobalId, SystemObjectMapping>,
    ) -> Result<(), Error> {
        let n = self.system_gid_mapping.update(|_k, v| {
            if let Some(mapping) = mappings.get(&GlobalId::System(v.id)) {
                let id = if let GlobalId::System(id) = mapping.unique_identifier.id {
                    id
                } else {
                    panic!("non-system id provided")
                };
                Some(GidMappingValue {
                    id,
                    fingerprint: mapping.unique_identifier.fingerprint.clone(),
                })
            } else {
                None
            }
        })?;

        if usize::try_from(n).expect("update diff should fit into usize") != mappings.len() {
            let id_str = mappings.keys().map(|id| id.to_string()).join(",");
            return Err(SqlCatalogError::FailedBuiltinSchemaMigration(id_str).into());
        }

        Ok(())
    }

    /// Updates cluster `id` in the transaction to `cluster`.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of clusters in the stash.
    /// DO NOT call this function in a loop.
    pub fn update_cluster(&mut self, id: ClusterId, cluster: Cluster) -> Result<(), Error> {
        let n = self.clusters.update(|k, _v| {
            if k.id == id {
                let cluster = cluster.clone();
                Some(ClusterValue {
                    name: cluster.name,
                    linked_object_id: cluster.linked_object_id,
                    owner_id: cluster.owner_id,
                    privileges: cluster.privileges,
                    config: cluster.config,
                })
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownCluster(id.to_string()).into())
        }
    }

    /// Updates cluster replica `replica_id` in the transaction to `replica`.
    ///
    /// Returns an error if `replica_id` is not found.
    ///
    /// Runtime is linear with respect to the total number of cluster replicas in the stash.
    /// DO NOT call this function in a loop.
    pub fn update_cluster_replica(
        &mut self,
        cluster_id: ClusterId,
        replica_id: ReplicaId,
        replica: ClusterReplica,
    ) -> Result<(), Error> {
        let n = self.cluster_replicas.update(|k, _v| {
            if k.id == replica_id {
                let replica = replica.clone();
                Some(ClusterReplicaValue {
                    cluster_id,
                    name: replica.name,
                    config: replica.config,
                    owner_id: replica.owner_id,
                })
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownClusterReplica(replica_id.to_string()).into())
        }
    }

    /// Updates database `id` in the transaction to `database`.
    ///
    /// Returns an error if `id` is not found.
    ///
    /// Runtime is linear with respect to the total number of databases in the stash.
    /// DO NOT call this function in a loop.
    pub fn update_database(&mut self, id: DatabaseId, database: Database) -> Result<(), Error> {
        let n = self.databases.update(|k, _v| {
            if id == k.id {
                let database = database.clone();
                Some(DatabaseValue {
                    name: database.name,
                    owner_id: database.owner_id,
                    privileges: database.privileges,
                })
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownDatabase(id.to_string()).into())
        }
    }

    /// Updates schema `schema_id` in the transaction to `schema`.
    ///
    /// Returns an error if `schema_id` is not found.
    ///
    /// Runtime is linear with respect to the total number of schemas in the stash.
    /// DO NOT call this function in a loop.
    pub fn update_schema(
        &mut self,
        database_id: Option<DatabaseId>,
        schema_id: SchemaId,
        schema: Schema,
    ) -> Result<(), Error> {
        let n = self.schemas.update(|k, _v| {
            if schema_id == k.id {
                let schema = schema.clone();
                Some(SchemaValue {
                    database_id,
                    name: schema.name,
                    owner_id: schema.owner_id,
                    privileges: schema.privileges,
                })
            } else {
                None
            }
        })?;
        assert!(n <= 1);
        if n == 1 {
            Ok(())
        } else {
            Err(SqlCatalogError::UnknownSchema(schema_id.to_string()).into())
        }
    }

    /// Set persisted default privilege.
    pub fn set_default_privilege(
        &mut self,
        role_id: RoleId,
        database_id: Option<DatabaseId>,
        schema_id: Option<SchemaId>,
        object_type: ObjectType,
        grantee: RoleId,
        privileges: Option<AclMode>,
    ) -> Result<(), Error> {
        self.default_privileges.set(
            DefaultPrivilegesKey {
                role_id,
                database_id,
                schema_id,
                object_type,
                grantee,
            },
            privileges.map(|privileges| DefaultPrivilegesValue { privileges }),
        )?;
        Ok(())
    }

    /// Set persisted system privilege.
    pub fn set_system_privilege(
        &mut self,
        grantee: RoleId,
        grantor: RoleId,
        acl_mode: Option<AclMode>,
    ) -> Result<(), Error> {
        self.system_privileges.set(
            SystemPrivilegesKey { grantee, grantor },
            acl_mode.map(|acl_mode| SystemPrivilegesValue { acl_mode }),
        )?;
        Ok(())
    }

    pub fn update_comment(
        &mut self,
        object_id: CommentObjectId,
        sub_component: Option<usize>,
        comment: Option<String>,
    ) -> Result<(), Error> {
        let key = CommentKey {
            object_id,
            sub_component,
        };
        let value = comment.map(|c| CommentValue { comment: c });
        self.comments.set(key, value)?;

        Ok(())
    }

    pub fn drop_comments(
        &mut self,
        object_id: CommentObjectId,
    ) -> Result<Vec<(CommentObjectId, Option<usize>, String)>, Error> {
        let deleted = self.comments.delete(|k, _v| k.object_id == object_id);
        let deleted = deleted
            .into_iter()
            .map(|(k, v)| (k.object_id, k.sub_component, v.comment))
            .collect();
        Ok(deleted)
    }

    /// Upserts persisted system configuration `name` to `value`.
    pub fn upsert_system_config(&mut self, name: &str, value: String) -> Result<(), Error> {
        let key = ServerConfigurationKey {
            name: name.to_string(),
        };
        let value = ServerConfigurationValue { value };
        self.system_configurations.set(key, Some(value))?;
        Ok(())
    }

    /// Removes persisted system configuration `name`.
    pub fn remove_system_config(&mut self, name: &str) {
        let key = ServerConfigurationKey {
            name: name.to_string(),
        };
        self.system_configurations
            .set(key, None)
            .expect("cannot have uniqueness violation");
    }

    /// Removes all persisted system configurations.
    pub fn clear_system_configs(&mut self) {
        self.system_configurations.delete(|_k, _v| true);
    }

    pub fn remove_timestamp(&mut self, timeline: Timeline) {
        let timeline_str = timeline.to_string();
        let prev = self
            .timestamps
            .set(TimestampKey { id: timeline_str }, None)
            .expect("cannot have uniqueness violation");
        assert!(prev.is_some());
    }

    /// Commits the storage transaction to the stash. Any error returned indicates the stash may be
    /// in an indeterminate state and needs to be fully re-read before proceeding. In general, this
    /// must be fatal to the calling process. We do not panic/halt inside this function itself so
    /// that errors can bubble up during initialization.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn commit(self) -> Result<(), Error> {
        let txn_batch = TransactionBatch {
            databases: self.databases.pending(),
            schemas: self.schemas.pending(),
            items: self.items.pending(),
            comments: self.comments.pending(),
            roles: self.roles.pending(),
            clusters: self.clusters.pending(),
            cluster_replicas: self.cluster_replicas.pending(),
            introspection_sources: self.introspection_sources.pending(),
            id_allocator: self.id_allocator.pending(),
            configs: self.configs.pending(),
            settings: self.settings.pending(),
            timestamps: self.timestamps.pending(),
            system_gid_mapping: self.system_gid_mapping.pending(),
            system_configurations: self.system_configurations.pending(),
            default_privileges: self.default_privileges.pending(),
            system_privileges: self.system_privileges.pending(),
            audit_log_updates: self.audit_log_updates,
            storage_usage_updates: self.storage_usage_updates,
        };
        self.durable_catalog.commit_transaction(txn_batch).await
    }
}

/// Describes a set of changes to apply as the result of a catalog transaction.
#[derive(Debug, Clone)]
pub struct TransactionBatch {
    pub(crate) databases: Vec<(proto::DatabaseKey, proto::DatabaseValue, Diff)>,
    pub(crate) schemas: Vec<(proto::SchemaKey, proto::SchemaValue, Diff)>,
    pub(crate) items: Vec<(proto::ItemKey, proto::ItemValue, Diff)>,
    pub(crate) comments: Vec<(proto::CommentKey, proto::CommentValue, Diff)>,
    pub(crate) roles: Vec<(proto::RoleKey, proto::RoleValue, Diff)>,
    pub(crate) clusters: Vec<(proto::ClusterKey, proto::ClusterValue, Diff)>,
    pub(crate) cluster_replicas: Vec<(proto::ClusterReplicaKey, proto::ClusterReplicaValue, Diff)>,
    pub(crate) introspection_sources: Vec<(
        proto::ClusterIntrospectionSourceIndexKey,
        proto::ClusterIntrospectionSourceIndexValue,
        Diff,
    )>,
    pub(crate) id_allocator: Vec<(proto::IdAllocKey, proto::IdAllocValue, Diff)>,
    pub(crate) configs: Vec<(proto::ConfigKey, proto::ConfigValue, Diff)>,
    pub(crate) settings: Vec<(proto::SettingKey, proto::SettingValue, Diff)>,
    pub(crate) timestamps: Vec<(proto::TimestampKey, proto::TimestampValue, Diff)>,
    pub(crate) system_gid_mapping: Vec<(proto::GidMappingKey, proto::GidMappingValue, Diff)>,
    pub(crate) system_configurations: Vec<(
        proto::ServerConfigurationKey,
        proto::ServerConfigurationValue,
        Diff,
    )>,
    pub(crate) default_privileges: Vec<(
        proto::DefaultPrivilegesKey,
        proto::DefaultPrivilegesValue,
        Diff,
    )>,
    pub(crate) system_privileges: Vec<(
        proto::SystemPrivilegesKey,
        proto::SystemPrivilegesValue,
        Diff,
    )>,
    pub(crate) audit_log_updates: Vec<(proto::AuditLogKey, (), Diff)>,
    pub(crate) storage_usage_updates: Vec<(proto::StorageUsageKey, (), Diff)>,
}
