//
// Copyright (c) Pirmin Kalberer. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root for full license information.
//

use crate::core::config::DatasourceCfg;
use crate::core::feature::Feature;
use crate::core::layer::Layer;
use crate::core::Config;
use crate::datasource::postgis_fields::FeatureRow;
use crate::datasource::DatasourceType;
use fallible_iterator::FallibleIterator;
use postgres::types::{self, ToSql};
use postgres_native_tls::NativeTls;
use r2d2;
use r2d2_postgres::{PostgresConnectionManager, TlsMode};
use std;
use std::collections::BTreeMap;
use tile_grid::Extent;
use tile_grid::Grid;

#[derive(PartialEq, Clone, Debug)]
pub enum QueryParam {
    Bbox,
    Zoom,
    PixelWidth,
    ScaleDenominator,
}

#[derive(Clone, Debug)]
pub struct SqlQuery {
    pub sql: String,
    pub params: Vec<QueryParam>,
}

#[derive(Clone)]
pub struct PostgisDatasource {
    pub connection_url: String,
    pub pool_size: Option<u16>,
    conn_pool: Option<r2d2::Pool<PostgresConnectionManager>>,
    // Queries for all tileset/layers and zoom levels
    queries: BTreeMap<String, BTreeMap<String, BTreeMap<u8, SqlQuery>>>,
}

impl SqlQuery {
    /// Replace variables (!bbox!, !zoom!, etc.) in query
    // https://github.com/mapnik/mapnik/wiki/PostGIS
    fn replace_params(&mut self, bbox_expr: String) {
        let mut numvars = 0;
        if self.sql.contains("!bbox!") {
            self.params.push(QueryParam::Bbox);
            numvars += 4;
            self.sql = self.sql.replace("!bbox!", &bbox_expr);
        }
        // replace e.g. !zoom! with $5
        for (var, par, cast) in vec![
            ("!zoom!", QueryParam::Zoom, ""),
            ("!pixel_width!", QueryParam::PixelWidth, "FLOAT8"),
            (
                "!scale_denominator!",
                QueryParam::ScaleDenominator,
                "FLOAT8",
            ),
        ] {
            if self.sql.contains(var) {
                self.params.push(par);
                numvars += 1;
                if cast != "" {
                    self.sql = self.sql.replace(var, &format!("${}::{}", numvars, cast));
                } else {
                    self.sql = self.sql.replace(var, &format!("${}", numvars));
                }
            }
        }
    }
    fn valid_sql_for_params(sql: &String) -> String {
        sql.replace("!bbox!", "ST_MakeEnvelope(0,0,0,0,3857)")
            .replace("!zoom!", "0")
            .replace("!pixel_width!", "0")
            .replace("!scale_denominator!", "0")
    }
}

impl PostgisDatasource {
    pub fn new(connection_url: &str, pool_size: Option<u16>) -> PostgisDatasource {
        PostgisDatasource {
            connection_url: connection_url.to_string(),
            pool_size,
            conn_pool: None,
            queries: BTreeMap::new(),
        }
    }
    fn conn(&self) -> r2d2::PooledConnection<PostgresConnectionManager> {
        let pool = self.conn_pool.as_ref().unwrap();
        //debug!("{:?}", pool);
        // Waits for at most Config::connection_timeout (default: 30s) before returning an error.
        pool.get().unwrap()
    }
    pub fn detect_geometry_types(&self, layer: &Layer) -> Vec<String> {
        let field = layer
            .geometry_field
            .as_ref()
            .expect("geometry_field undefined");
        let table = layer.table_name.as_ref().expect("geometry_type undefined");
        info!(
            "Detecting geometry types for field '{}' in table {} (use --detect-geometry-types=false to skip)",
            field, table
        );

        let conn = self.conn();
        let sql = format!(
            "SELECT DISTINCT GeometryType({}) AS geomtype FROM {}",
            field, table
        );

        let mut types: Vec<String> = Vec::new();
        for row in &conn.query(&sql, &[]).unwrap() {
            let geomtype = row.get_opt("geomtype");
            match geomtype.unwrap() {
                Ok(Some(val)) => {
                    types.push(val);
                }
                Ok(None) => {
                    warn!(
                        "Ignoring unknown geometry types for field '{}' in table {}",
                        field, table
                    );
                }
                Err(err) => {
                    warn!(
                        "Error in type detection for field '{}' in table {}: {}",
                        field, table, err
                    );
                }
            }
        }
        types
    }
    /// Return column field names and Rust compatible type conversion
    pub fn detect_columns(&self, layer: &Layer, sql: Option<&String>) -> Vec<(String, String)> {
        let mut query = match sql {
            Some(&ref userquery) => userquery.clone(),
            None => format!(
                "SELECT * FROM {}",
                layer.table_name.as_ref().unwrap_or(&layer.name)
            ),
        };
        query = SqlQuery::valid_sql_for_params(&query);
        let conn = self.conn();
        let stmt = conn.prepare(&query);
        match stmt {
            Err(e) => {
                error!("Layer '{}': {}", layer.name, e);
                vec![]
            }
            Ok(stmt) => {
                let cols: Vec<(String, String)> = stmt
                    .columns()
                    .iter()
                    .map(|col| {
                        let name = col.name().to_string();
                        let ty = col.type_();
                        let cast = match ty {
                            &types::VARCHAR
                            | &types::VARCHAR_ARRAY
                            | &types::TEXT
                            | &types::CHAR_ARRAY
                            | &types::FLOAT4
                            | &types::FLOAT8
                            | &types::INT2
                            | &types::INT4
                            | &types::INT8
                            | &types::BOOL => String::new(),
                            &types::NUMERIC => "FLOAT8".to_string(),
                            _ => match ty.name() {
                                "geometry" => String::new(),
                                _ => "TEXT".to_string(),
                            },
                        };
                        if !cast.is_empty() {
                            warn!(
                                "Layer '{}': Converting field '{}' of type {} to {}",
                                layer.name,
                                name,
                                col.type_().name(),
                                cast
                            );
                        }
                        (name, cast)
                    })
                    .collect();
                let _ = stmt.finish();
                cols
            }
        }
    }
    /// Execute query returning an extent as polygon
    fn extent_query(&self, sql: String) -> Option<Extent> {
        use postgis::ewkb;
        use postgis::{LineString, Point, Polygon}; // conflicts with core::geom::Point etc.

        let conn = self.conn();
        let rows = conn.query(&sql, &[]).unwrap();
        let extpoly = rows
            .into_iter()
            .nth(0)
            .expect("row expected")
            .get_opt::<_, ewkb::Polygon>("extent");
        match extpoly {
            Some(Ok(ref poly)) if poly.rings().len() != 1 => None,
            Some(Ok(poly)) => {
                let p1 = poly.rings().nth(0).unwrap().points().nth(0).unwrap();
                let p2 = poly.rings().nth(0).unwrap().points().nth(2).unwrap();
                Some(Extent {
                    minx: p1.x(),
                    miny: p1.y(),
                    maxx: p2.x(),
                    maxy: p2.y(),
                })
            }
            _ => None,
        }
    }
    /// Build geometry selection expression for feature query.
    fn build_geom_expr(&self, layer: &Layer, grid_srid: i32, zoom: u8) -> String {
        let layer_srid = layer.srid.unwrap_or(0);
        let ref geom_name = layer
            .geometry_field
            .as_ref()
            .expect("geometry_field undefined");
        let mut geom_expr = String::from(geom_name as &str);

        // Convert special geometry types like curves
        match layer
            .geometry_type
            .as_ref()
            .unwrap_or(&"GEOMETRY".to_string()) as &str
        {
            "CURVEPOLYGON" | "COMPOUNDCURVE" => {
                geom_expr = format!("ST_CurveToLine({})", geom_expr);
            }
            _ => {}
        };

        // Clipping
        if layer.buffer_size.is_some() {
            let valid_geom = if layer.make_valid {
                format!("ST_MakeValid({})", geom_expr)
            } else {
                geom_expr.clone()
            };
            match layer
                .geometry_type
                .as_ref()
                .unwrap_or(&"GEOMETRY".to_string()) as &str
            {
                "POLYGON" | "MULTIPOLYGON" | "CURVEPOLYGON" => {
                    geom_expr = format!("ST_Buffer(ST_Intersection({},!bbox!), 0.0)", valid_geom);
                }
                "POINT" => {
                    // ST_Intersection not necessary - bbox query in WHERE clause is sufficient
                }
                _ => {
                    geom_expr = format!("ST_Intersection({},!bbox!)", valid_geom);
                } //Buffer is added to !bbox! when replaced
            };
        }

        // convert LINESTRING and POLYGON to multi geometries (and fix potential (empty) single types)
        match layer
            .geometry_type
            .as_ref()
            .unwrap_or(&"GEOMETRY".to_string()) as &str
        {
            "MULTIPOINT" | "LINESTRING" | "MULTILINESTRING" | "COMPOUNDCURVE" | "POLYGON"
            | "MULTIPOLYGON" | "CURVEPOLYGON" => {
                geom_expr = format!("ST_Multi({})", geom_expr);
            }
            _ => {}
        }

        // Simplify
        if layer.simplify(zoom) {
            geom_expr = match layer
                .geometry_type
                .as_ref()
                .unwrap_or(&"GEOMETRY".to_string()) as &str
            {
                "LINESTRING" | "MULTILINESTRING" | "COMPOUNDCURVE" => format!(
                    "ST_Multi(ST_SimplifyPreserveTopology({},{}))",
                    geom_expr,
                    layer.tolerance(zoom)
                ),
                "POLYGON" | "MULTIPOLYGON" | "CURVEPOLYGON" => {
                    let empty_geom =
                        format!("ST_GeomFromText('MULTIPOLYGON EMPTY',{})", layer_srid);
                    format!(
                        "COALESCE(ST_MakeValid(ST_SnapToGrid({}, {})),{})::geometry(MULTIPOLYGON,{})",
                        geom_expr,
                        layer.tolerance(zoom),
                        empty_geom,
                        layer_srid
                    )
                }
                _ => geom_expr, // No simplification for points or unknown types
            };
        }

        // Transform geometry to grid SRID
        if layer_srid <= 0 {
            warn!(
                "Layer '{}': Unknown SRS of geometry '{}' - assuming SRID {}",
                layer.name, geom_name, grid_srid
            );
            geom_expr = format!("ST_SetSRID({},{})", geom_expr, grid_srid)
        } else if layer_srid != grid_srid {
            if layer.no_transform {
                geom_expr = format!("ST_SetSRID({},{})", geom_expr, grid_srid);
            } else {
                info!(
                    "Layer '{}': Reprojecting geometry '{}' from SRID {} to {}",
                    layer.name, geom_name, layer_srid, grid_srid
                );
                geom_expr = format!("ST_Transform({},{})", geom_expr, grid_srid);
            }
        }

        if geom_expr.starts_with("ST_") || geom_expr.starts_with("COALESCE") {
            geom_expr = format!("{} AS {}", geom_expr, geom_name);
        }

        geom_expr
    }
    /// Build select list expressions for feature query.
    fn build_select_list(&self, layer: &Layer, geom_expr: String, sql: Option<&String>) -> String {
        let offline = self.conn_pool.is_none();
        if offline {
            geom_expr
        } else {
            let mut cols: Vec<String> = self
                .detect_data_columns(layer, sql)
                .iter()
                .map(|&(ref name, ref casttype)| {
                    // Wrap column names in double quotes to guarantee validity. Columns might have colons
                    if casttype.is_empty() {
                        format!("\"{}\"", name)
                    } else {
                        format!("\"{}\"::{}", name, casttype)
                    }
                })
                .collect();
            cols.insert(0, geom_expr);
            cols.join(",")
        }
    }
    /// Build !bbox! replacement expression for feature query.
    fn build_bbox_expr(&self, layer: &Layer, grid_srid: i32) -> String {
        let layer_srid = layer.srid.unwrap_or(grid_srid); // we assume grid srid as default
        let env_srid = if layer_srid <= 0 || layer.no_transform {
            layer_srid
        } else {
            grid_srid
        };
        let mut expr = format!("ST_MakeEnvelope($1,$2,$3,$4,{})", env_srid);
        if let Some(pixels) = layer.buffer_size {
            if pixels != 0 {
                expr = format!("ST_Buffer({},{}*!pixel_width!)", expr, pixels);
            }
        }
        if layer_srid > 0 && layer_srid != env_srid && !layer.no_transform {
            expr = format!("ST_Transform({},{})", expr, layer_srid);
        }
        // Clip bbox to maximal extent of SRID
        if layer.shift_longitude {
            expr = format!("ST_Shift_Longitude({})", expr);
        }
        expr
    }
    /// Build feature query SQL (also used for generated config).
    pub fn build_query_sql(
        &self,
        layer: &Layer,
        grid_srid: i32,
        zoom: u8,
        sql: Option<&String>,
        raw_geom: bool,
    ) -> Option<String> {
        let mut query;
        let offline = self.conn_pool.is_none();
        let ref geom_name = layer
            .geometry_field
            .as_ref()
            .expect("geometry_field undefined");
        let geom_expr = if raw_geom {
            // Skip geometry processing when generating user query template
            geom_name.to_string()
        } else {
            self.build_geom_expr(layer, grid_srid, zoom)
        };
        let select_list = self.build_select_list(layer, geom_expr, sql);
        let intersect_clause = format!(" WHERE {} && !bbox!", geom_name);

        if let Some(&ref userquery) = sql {
            // user query
            let ref select = if offline {
                "*".to_string()
            } else {
                select_list
            };
            query = format!("SELECT {} FROM ({}) AS _q", select, userquery);
            if !userquery.contains("!bbox!") {
                query.push_str(&intersect_clause);
            }
        } else {
            // automatic query
            if layer.table_name.is_none() {
                return None;
            }
            query = format!(
                "SELECT {} FROM {}",
                select_list,
                layer.table_name.as_ref().expect("table_name undefined")
            );
            query.push_str(&intersect_clause);
        };

        Some(query)
    }
    pub fn build_query(
        &self,
        layer: &Layer,
        grid_srid: i32,
        zoom: u8,
        sql: Option<&String>,
    ) -> Option<SqlQuery> {
        let sqlquery = self.build_query_sql(layer, grid_srid, zoom, sql, false);
        if sqlquery.is_none() {
            return None;
        }
        let bbox_expr = self.build_bbox_expr(layer, grid_srid);
        let mut query = SqlQuery {
            sql: sqlquery.expect("sqlquery expected"),
            params: Vec::new(),
        };
        query.replace_params(bbox_expr);
        Some(query)
    }
    fn query(&self, tileset: &String, layer: &String, zoom: u8) -> Option<&SqlQuery> {
        let ref queries = self
            .queries
            .get(tileset)
            .expect("Tileset query lookup failed")
            .get(layer)
            .expect("Layer query lookup failed");
        queries.get(&zoom)
    }
}

impl DatasourceType for PostgisDatasource {
    /// New instance with connected pool
    fn connected(&self) -> PostgisDatasource {
        // Emulate TlsMode::Allow (https://github.com/sfackler/rust-postgres/issues/278)
        let manager =
            PostgresConnectionManager::new(self.connection_url.as_ref(), TlsMode::None).unwrap();
        let pool_size = self.pool_size.unwrap_or(8); // TODO: use number of workers as default pool size
        let pool = r2d2::Pool::builder()
            .max_size(pool_size as u32)
            .build(manager)
            .or_else(|e| match &e.to_string() as &str {
                "unable to initialize connections" => {
                    info!("Couldn't connect with TlsMode::None - retrying with TlsMode::Require");
                    let negotiator = NativeTls::new().unwrap();
                    let manager = PostgresConnectionManager::new(
                        self.connection_url.as_ref(),
                        TlsMode::Require(Box::new(negotiator)),
                    )
                    .unwrap();
                    r2d2::Pool::builder()
                        .max_size(pool_size as u32)
                        .build(manager)
                }
                _ => Err(e),
            })
            .unwrap();
        PostgisDatasource {
            connection_url: self.connection_url.clone(),
            pool_size: Some(pool_size),
            conn_pool: Some(pool),
            queries: BTreeMap::new(),
        }
    }
    fn detect_layers(&self, detect_geometry_types: bool) -> Vec<Layer> {
        info!("Detecting layers from geometry_columns");
        let mut layers: Vec<Layer> = Vec::new();
        let conn = self.conn();
        let sql = "SELECT * FROM geometry_columns ORDER BY f_table_schema,f_table_name DESC";
        for row in &conn.query(sql, &[]).unwrap() {
            let schema: String = row.get("f_table_schema");
            let table_name: String = row.get("f_table_name");
            let geometry_column: String = row.get("f_geometry_column");
            let srid: i32 = row.get("srid");
            let geomtype: String = row.get("type");
            let mut layer = Layer::new(&table_name);
            layer.table_name = if schema != "public" {
                Some(format!("\"{}\".\"{}\"", schema, table_name))
            } else {
                Some(format!("\"{}\"", table_name))
            };
            layer.geometry_field = Some(geometry_column.clone());
            layer.geometry_type = match &geomtype as &str {
                "GEOMETRY" => {
                    if detect_geometry_types {
                        let field = layer
                            .geometry_field
                            .as_ref()
                            .expect("geometry_field undefined");
                        let table = layer.table_name.as_ref().expect("table_name undefined");
                        let types = self.detect_geometry_types(&layer);
                        if types.len() == 1 {
                            debug!(
                                "Detected unique geometry type in '{}.{}': {}",
                                table, field, &types[0]
                            );
                            Some(types[0].clone())
                        } else {
                            let type_list = types.join(", ");
                            warn!(
                                "Multiple geometry types in {}.{}: {}",
                                table, field, type_list
                            );
                            Some("GEOMETRY".to_string())
                        }
                    } else {
                        warn!(
                            "Unknwon geometry type of {}.{}",
                            table_name, geometry_column
                        );
                        Some("GEOMETRY".to_string())
                    }
                }
                _ => Some(geomtype.clone()),
            };
            layer.srid = Some(srid);
            layers.push(layer);
        }
        layers
    }
    /// Return column field names and Rust compatible type conversion - without geometry column
    fn detect_data_columns(&self, layer: &Layer, sql: Option<&String>) -> Vec<(String, String)> {
        debug!(
            "detect_data_columns for layer {} with sql {:?}",
            layer.name, sql
        );
        let cols = self.detect_columns(layer, sql);
        let filter_cols = vec![layer
            .geometry_field
            .as_ref()
            .expect("geometry_field undefined")];
        cols.into_iter()
            .filter(|&(ref col, _)| !filter_cols.contains(&&col))
            .collect()
    }
    /// Projected extent
    fn extent_from_wgs84(&self, extent: &Extent, dest_srid: i32) -> Option<Extent> {
        let sql = format!(
            "SELECT ST_Transform(ST_MakeEnvelope({}, {}, {}, {}, 4326), {}) AS extent",
            extent.minx, extent.miny, extent.maxx, extent.maxy, dest_srid
        );
        self.extent_query(sql)
    }
    /// Detect extent of layer (in WGS84)
    fn layer_extent(&self, layer: &Layer, grid_srid: i32) -> Option<Extent> {
        let ref geom_name = layer
            .geometry_field
            .as_ref()
            .expect("geometry_field undefined");
        let src_srid = if layer.no_transform {
            // Shift coordinates to display extent in grid SRS
            grid_srid
        } else {
            layer.srid.unwrap_or(0)
        };
        if !layer.query.is_empty() || src_srid <= 0 {
            info!(
                "Couldn't detect extent of layer {}, because of custom queries or an unknown SRID",
                layer.name
            );
            return None;
        }
        let extent_sql = format!(
            "ST_Transform(ST_SetSRID(ST_Extent({}),{}),4326)",
            geom_name, src_srid
        );
        let sql = format!(
            "SELECT {} AS extent FROM {}",
            extent_sql,
            layer.table_name.as_ref().expect("table_name undefined")
        );
        self.extent_query(sql)
    }
    fn prepare_queries(&mut self, tileset: &str, layer: &Layer, grid_srid: i32) {
        let mut queries = BTreeMap::new();

        // Configuration checks (TODO: add config_check to trait)
        if layer.geometry_field.is_none() {
            error!("Layer '{}': geometry_field undefined", layer.name);
        }
        if layer.query.len() == 0 && layer.table_name.is_none() {
            error!("Layer '{}': table_name undefined", layer.name);
        }

        for zoom in layer.minzoom()..=layer.maxzoom(22) {
            let layer_query = layer.query(zoom);
            if let Some(query) = self.build_query(layer, grid_srid, zoom, layer_query) {
                debug!("Query for layer '{}': {}", layer.name, query.sql);
                queries.insert(zoom, query.clone());
            }
        }

        // Insert into self.queries
        self.queries
            .entry(tileset.to_string())
            .or_insert(BTreeMap::new())
            .insert(layer.name.clone(), queries);
    }
    fn retrieve_features<F>(
        &self,
        tileset: &str,
        layer: &Layer,
        extent: &Extent,
        zoom: u8,
        grid: &Grid,
        mut read: F,
    ) -> u64
    where
        F: FnMut(&dyn Feature),
    {
        let conn = self.conn();
        let query = self.query(&tileset.to_string(), &layer.name, zoom);
        if query.is_none() {
            return 0;
        }
        let query = query.unwrap();
        let stmt = conn.prepare_cached(&query.sql);
        if let Err(err) = stmt {
            error!("Layer '{}': {}", layer.name, err);
            error!("Query: {}", query.sql);
            return 0;
        };

        // Add query params
        let zoom_param = zoom as i32;
        let pixel_width = grid.pixel_width(zoom); //TODO: calculate only if needed
        let scale_denominator = grid.scale_denominator(zoom);
        let mut params = Vec::new();
        for param in &query.params {
            match param {
                &QueryParam::Bbox => {
                    let mut bbox: Vec<&dyn ToSql> =
                        vec![&extent.minx, &extent.miny, &extent.maxx, &extent.maxy];
                    params.append(&mut bbox);
                }
                &QueryParam::Zoom => params.push(&zoom_param),
                &QueryParam::PixelWidth => params.push(&pixel_width),
                &QueryParam::ScaleDenominator => {
                    params.push(&scale_denominator);
                }
            }
        }

        let stmt = stmt.unwrap();
        let trans = conn.transaction().expect("transaction already active");
        let rows = stmt.lazy_query(&trans, &params.as_slice(), 50);
        if let Err(err) = rows {
            error!("Layer '{}': {}", layer.name, err);
            error!("Query: {}", query.sql);
            error!("Param types: {:?}", query.params);
            error!("Param values: {:?}", params);
            return 0;
        };
        debug!("Reading features in layer {}", layer.name);
        let mut cnt = 0;
        let query_limit = layer.query_limit.unwrap_or(0);
        for row in rows.unwrap().iterator() {
            let feature = FeatureRow {
                layer: layer,
                row: &row.unwrap(),
            };
            read(&feature);
            cnt += 1;
            if cnt == query_limit as u64 {
                info!(
                    "Features of layer {} limited to {} (tile query_limit reached, zoom level {})",
                    layer.name, cnt, zoom
                );
                break;
            }
        }
        cnt
    }
}

impl<'a> Config<'a, DatasourceCfg> for PostgisDatasource {
    fn from_config(ds_cfg: &DatasourceCfg) -> Result<Self, String> {
        Ok(PostgisDatasource::new(
            ds_cfg.dbconn.as_ref().unwrap(),
            ds_cfg.pool,
        ))
    }

    fn gen_config() -> String {
        let toml = r#"
[[datasource]]
name = "database"
# PostgreSQL connection specification (https://github.com/sfackler/rust-postgres#connecting)
dbconn = "postgresql://user:pass@host/database"
"#;
        toml.to_string()
    }
    fn gen_runtime_config(&self) -> String {
        format!(
            r#"
[[datasource]]
dbconn = "{}"
"#,
            self.connection_url
        )
    }
}
