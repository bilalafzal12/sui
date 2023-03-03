use std::{io::stdout, time::Duration};

use crossterm::{
    cursor::MoveToColumn,
    style::{Print, Stylize},
};
use futures::future::try_join_all;
use prettytable::{format, row, Table};
use tokio::time::sleep;

use crate::{
    client::ServerProviderClient,
    ensure,
    error::{TestbedError, TestbedResult},
    settings::Settings,
    ssh::SshConnection,
};

use super::client::Instance;

pub struct Testbed<C> {
    /// The testbed's settings.
    settings: Settings,
    /// The client interfacing with the cloud provider.
    client: C,
    /// The state of the testbed (reflecting accurately the state of the machines).
    instances: Vec<Instance>,
}

impl<C: ServerProviderClient> Testbed<C> {
    /// Create a new testbed instance with the specified settings and client.
    pub async fn new(settings: Settings, client: C) -> TestbedResult<Self> {
        let public_key = settings.load_ssh_public_key()?;
        client.register_ssh_public_key(public_key).await?;
        let instances = client.list_instances().await?;

        Ok(Self {
            settings,
            client,
            instances,
        })
    }

    pub fn instances(&self) -> Vec<Instance> {
        self.instances.clone()
    }

    pub fn username(&self) -> &'static str {
        C::USERNAME
    }

    /// Print the current status of the testbed.
    pub fn status(&self) {
        let sorted: Vec<(_, Vec<_>)> = self
            .settings
            .regions
            .iter()
            .map(|region| {
                (
                    region,
                    self.instances
                        .iter()
                        .filter(|instance| &instance.region == region)
                        .collect(),
                )
            })
            .collect();

        println!();
        println!("{} {}", "Client:".bold(), self.client);
        println!(
            "{} {} ({})",
            "Repo:".bold(),
            self.settings.repository.url,
            self.settings.repository.branch
        );

        let mut table = Table::new();
        let format = format::FormatBuilder::new()
            .separators(
                &[
                    format::LinePosition::Top,
                    format::LinePosition::Bottom,
                    format::LinePosition::Title,
                ],
                format::LineSeparator::new('-', '-', '-', '-'),
            )
            .padding(1, 1)
            .build();
        table.set_format(format);

        println!();
        table.set_titles(row![bH2->format!("Instances ({})",self.instances.len())]);
        for (i, (region, instances)) in sorted.iter().enumerate() {
            table.add_row(row![bH2->region.to_uppercase()]);
            let mut j = 0;
            for instance in instances {
                if (j + 1) % 5 == 0 {
                    table.add_row(row![]);
                }
                let private_key_file = self.settings.ssh_private_key_file.display();
                let username = C::USERNAME;
                let ip = instance.main_ip;
                let connect = format!("ssh -i {private_key_file} {username}@{ip}");
                if !instance.is_terminated() {
                    if instance.is_active() {
                        table.add_row(row![bFg->format!("{j}"), connect]);
                    } else {
                        table.add_row(row![bFr->format!("{j}"), connect]);
                    }
                    j += 1;
                }
            }
            if i != sorted.len() - 1 {
                table.add_row(row![]);
            }
        }
        table.printstd();
        println!();
    }

    /// Populate the testbed by creating the specified amount of instances per region. The total
    /// number of instances created is thus the specified amount x the number of regions.
    pub async fn deploy(&mut self, quantity: usize) -> TestbedResult<()> {
        crossterm::execute!(
            stdout(),
            Print(format!(
                "Populating testbed with {quantity} instances per region..."
            ))
        )
        .unwrap();

        try_join_all(
            self.settings
                .regions
                .iter()
                .map(|region| (0..quantity).map(|_| self.client.create_instance(region.clone())))
                .flatten()
                .collect::<Vec<_>>(),
        )
        .await?;

        // Wait until the instances are booted.
        self.ready().await?;
        self.instances = self.client.list_instances().await?;
        Ok(())
    }

    /// Destroy all instances of the testbed.
    pub async fn destroy(&mut self) -> TestbedResult<()> {
        try_join_all(
            self.instances
                .drain(..)
                .map(|instance| self.client.delete_instance(instance))
                .collect::<Vec<_>>(),
        )
        .await
        .map_err(TestbedError::from)
        .map(|_| ())
    }

    /// Start the specified number of instances in each region. Returns an error if there are not
    /// enough available instances.
    pub async fn start(&mut self, quantity: usize) -> TestbedResult<()> {
        // Gather available instances.
        let mut available = Vec::new();
        let mut missing = Vec::new();

        for region in &self.settings.regions {
            let filtered: Vec<_> = self
                .instances
                .iter()
                .filter(|x| x.is_inactive() && &x.region == region)
                .take(quantity)
                .collect();
            if filtered.len() < quantity {
                missing.push((region.clone(), quantity - filtered.len()))
            } else {
                available.extend(filtered);
            }
        }

        ensure!(
            missing.is_empty(),
            TestbedError::InsufficientCapacity(format!("{missing:?}"))
        );

        // Start instances.
        self.client.start_instances(available.into_iter()).await?;

        // Wait until the instances are started.
        self.ready().await?;
        self.instances = self.client.list_instances().await?;
        Ok(())
    }

    /// Stop all instances of the testbed.
    pub async fn stop(&mut self) -> TestbedResult<()> {
        // Stop all instances.
        self.client.stop_instances(self.instances.iter()).await?;

        // Wait until the instances are stopped.
        loop {
            let instances = self.client.list_instances().await?;
            if instances.iter().all(|x| x.is_inactive()) {
                self.instances = instances;
                break;
            }
        }
        Ok(())
    }

    async fn ready(&self) -> TestbedResult<()> {
        let mut waiting = 0;
        loop {
            let duration = Duration::from_secs(5);
            sleep(duration).await;

            waiting += duration.as_secs();
            crossterm::execute!(
                stdout(),
                MoveToColumn(0),
                Print(format!("Waiting for machines to boot ({waiting}s)..."))
            )
            .unwrap();

            let instances = self.client.list_instances().await?;
            if try_join_all(instances.iter().map(|instance| {
                let private_key_file = self.settings.ssh_private_key_file.clone();
                SshConnection::new(instance.ssh_address(), C::USERNAME, private_key_file)
            }))
            .await
            .is_ok()
            {
                break;
            }
        }

        println!(" [{}]", "Ok".green());
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::{client::test_client::TestClient, settings::Settings, testbed::Testbed};

    #[tokio::test]
    async fn populate() {
        let settings = Settings::new_for_test();
        let client = TestClient::default();
        let mut testbed = Testbed::new(settings, client).await.unwrap();

        testbed.deploy(5).await.unwrap();

        assert_eq!(
            testbed.instances.len(),
            5 * testbed.settings.number_of_regions()
        );
        for (i, instance) in testbed.instances.iter().enumerate() {
            assert_eq!(i.to_string(), instance.id);
        }
    }

    #[tokio::test]
    async fn destroy() {
        let settings = Settings::new_for_test();
        let client = TestClient::default();
        let mut testbed = Testbed::new(settings, client).await.unwrap();

        testbed.destroy().await.unwrap();

        assert_eq!(testbed.instances.len(), 0);
    }

    #[tokio::test]
    async fn start() {
        let settings = Settings::new_for_test();
        let client = TestClient::default();
        let mut testbed = Testbed::new(settings, client).await.unwrap();
        testbed.deploy(5).await.unwrap();

        let result = testbed.start(2).await;

        assert!(result.is_ok());
        for region in &testbed.settings.regions {
            let active = testbed
                .instances
                .iter()
                .filter(|x| x.is_active() && &x.region == region)
                .count();
            assert_eq!(active, 2);

            let inactive = testbed
                .instances
                .iter()
                .filter(|x| x.is_inactive() && &x.region == region)
                .count();
            assert_eq!(inactive, 3);
        }
    }

    #[tokio::test]
    async fn start_insufficient_capacity() {
        let settings = Settings::new_for_test();
        let client = TestClient::default();
        let mut testbed = Testbed::new(settings, client).await.unwrap();
        testbed.deploy(1).await.unwrap();

        let result = testbed.start(2).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn stop() {
        let settings = Settings::new_for_test();
        let client = TestClient::default();
        let mut testbed = Testbed::new(settings, client).await.unwrap();
        testbed.deploy(5).await.unwrap();
        testbed.start(2).await.unwrap();

        testbed.stop().await.unwrap();

        assert!(testbed
            .instances
            .iter()
            .all(|x| x.power_status == "inactive"))
    }
}