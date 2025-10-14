"use client";

import { useState } from "react";
import { Button } from "@/components/ui/button";
import { usePathname, useRouter } from "next/navigation";
import Image from "next/image";
import Filter from "../../public/filter.svg";
import Close from "../../public/close.svg";
import Add from "../../public/add.svg";
import CapacityIcon from "../../public/capacity-icon.svg";

export type HeaderFilters = {
  paymentState?: "settled" | "failed" | "inflight";
  operator?: "gte" | "lte" | "eq";
  value?: number;
  from?: string;
  to?: string;
};

export function PaymentHeader({
  onApplyFilters,
}: {
  onApplyFilters?: (filters: HeaderFilters) => void;
}) {
  const pathname = usePathname();
  const router = useRouter();
  const last = pathname.split("/").pop() ?? "";
  const pageTitle = last ? last.charAt(0).toUpperCase() + last.slice(1) : "";

  const [showFilter, setShowFilter] = useState(false);
  const [isCapacityOpen, setIsCapacityOpen] = useState(false);
  const [isDateOpen, setIsDateOpen] = useState(false);
  const [isStateOpen, setIsStateOpen] = useState(false);

  const [capacityOperator, setCapacityOperator] = useState<
    "gte" | "lte" | "eq"
  >();
  const [capacityValue, setCapacityValue] = useState<string>("");
  const [dateFrom, setDateFrom] = useState<string>("");
  const [dateTo, setDateTo] = useState<string>("");
  const [paymentState, setPaymentState] = useState<
    "settled" | "failed" | "inflight"
  >();

  if (!pageTitle) return null;

  const resetFilterStates = () => {
    setCapacityOperator(undefined);
    setCapacityValue("");
    setDateFrom("");
    setDateTo("");
    setPaymentState(undefined);
    setIsCapacityOpen(false);
    setIsDateOpen(false);
    setIsStateOpen(false);
  };

  const handleApply = () => {
    if (
      !paymentState &&
      !capacityOperator &&
      !capacityValue &&
      !dateFrom &&
      !dateTo
    ) {
      router.push("/payments");
      window.location.reload();
      resetFilterStates();
      setShowFilter(false);
      return;
    }

    const filters: HeaderFilters = {};

    if (paymentState) filters.paymentState = paymentState;
    if (capacityOperator) filters.operator = capacityOperator;
    if (capacityValue) filters.value = Number(capacityValue);

    if (dateFrom) {
      filters.from = `${dateFrom}T00:00:00Z`;
    }
    if (dateTo) {
      filters.to = `${dateTo}T23:59:59Z`;
    }

    onApplyFilters?.(filters);
    resetFilterStates(); // Reset all filter UI states
    setShowFilter(false);
  };

  return (
    <div className="flex items-center justify-between mb-2 mt-4 font-clash-grotesk text-grey-dark">
      <h1 className="text-3xl font-medium">{pageTitle}</h1>

      <div className="flex items-center gap-4">
        <button
          className="flex items-center gap-2 text-sm border-[1px] font-[500] border-[#D4D4D4] bg-[#F7F7F7] rounded-[50px] text-[#294459] px-[25px] py-[10px]"
          onClick={() => setShowFilter(true)}
        >
          <Image src={Filter} alt="Filter" />
          <p>Filter</p>
        </button>

        {showFilter && (
          <div className="fixed inset-0 z-[9999] flex justify-end">
            {/* Overlay */}
            <div
              className="absolute inset-0 bg-black/20"
              onClick={() => setShowFilter(false)}
            />
            {/* Drawer */}
            <div className="relative h-full w-full max-w-[420px] bg-white shadow-xl flex flex-col">
              <div className="flex items-center justify-between p-6">
                <div className="flex items-center gap-2">
                  <Image
                    src={Filter}
                    alt="Filter"
                    className="bg-[#F7F7F7] 
                              border-[1px] 
                               border-[#D4D4D4]
                              rounded-[8px]
                              p-[2px]
                              w-[24px]"
                  />
                  <span className="font-[500] text-lg">Filter</span>
                </div>
                <div onClick={() => setShowFilter(false)} className="">
                  <Image src={Close} alt="Close" />
                </div>
              </div>
              {/* Drawer content goes here */}
              <div className="flex-1 overflow-y-auto px-6">
                {/* Example content */}
                <div className="flex justify-between items-center">
                  <button
                    className="bg-[#F6F6F6] my-[20px] h-9 rounded-full px-4
                      font-[500] text-[15px] flex
                      justify-center items-center gap-2"
                  >
                    <Image src={Add} alt="Add" />
                    <span className="text-sm font-medium">Add Filter</span>
                  </button>
                  <Button
                    className="h-9 rounded-full bg-[#204ECF] text-[#F1F9FF] px-4"
                    onClick={handleApply}
                  >
                    Apply Filter
                  </Button>
                </div>

                <div className="mb-6">
                  <button
                    type="button"
                    onClick={() => setIsCapacityOpen((v) => !v)}
                    className="bg-[#EFF6FF] rounded-[20px] flex justify-between px-[20px] py-[15px] w-full text-left"
                  >
                    <p className="text-sm font-semibold">Capacity</p>
                    <Image src={CapacityIcon} alt="Capicity Icon" />
                  </button>
                  {isCapacityOpen && (
                    <div className="space-y-2 mt-4">
                      <select
                        aria-label="Capacity"
                        className="w-full rounded-lg border border-[#D4D4D4] bg-white px-2 py-4 text-sm outline-none"
                        value={capacityOperator}
                        onChange={(e: React.ChangeEvent<HTMLSelectElement>) =>
                          setCapacityOperator(
                            e.target.value as "gte" | "lte" | "eq"
                          )
                        }
                      >
                        <option value="">Select operator</option>
                        <option value="gte">Is greater than or equal to</option>
                        {/* <option value="lte">Is less than or equal to</option>
                      <option value="eq">Is exactly</option> */}
                      </select>
                      <input
                        type="number"
                        placeholder="5,000"
                        className="w-full rounded-lg border border-[#D4D4D4] bg-white px-3 py-4 text-sm outline-none"
                        value={capacityValue}
                        onChange={(e) => setCapacityValue(e.target.value)}
                      />
                    </div>
                  )}
                </div>
                <div>
                  <button
                    type="button"
                    onClick={() => setIsDateOpen((v) => !v)}
                    className="bg-[#EFF6FF] rounded-[20px] flex justify-between px-[20px] py-[15px] mb-[20px] w-full text-left"
                  >
                    <p className="text-sm font-semibold">Date Range</p>
                    <Image src={CapacityIcon} alt="Capicity Icon" />
                  </button>
                  {isDateOpen && (
                    <div className="flex flex-col gap-2">
                      <label
                        htmlFor="From"
                        className="text-[#727A86] text-[15px] font-[400]"
                      >
                        From
                        <input
                          type="date"
                          className="w-full border rounded-lg px-[10px] py-2 "
                          placeholder="Select"
                          value={dateFrom}
                          onChange={(e) => setDateFrom(e.target.value)}
                        />
                      </label>
                      <label
                        htmlFor="From"
                        className="text-[#727A86] text-[15px] font-[400]"
                      >
                        To
                        <input
                          type="date"
                          className="w-full border rounded-lg px-[10px] py-2 "
                          placeholder="Select"
                          value={dateTo}
                          onChange={(e) => setDateTo(e.target.value)}
                        />
                      </label>
                    </div>
                  )}
                </div>

                <div className="mt-6">
                  <button
                    type="button"
                    onClick={() => setIsStateOpen((v) => !v)}
                    className="bg-[#EFF6FF] rounded-[20px] flex justify-between px-[20px] py-[15px] w-full text-left"
                  >
                    <p className="text-sm font-semibold">State</p>
                    <Image src={CapacityIcon} alt="Capicity Icon" />
                  </button>
                  {isStateOpen && (
                    <div className="space-y-2 mt-4">
                      <select
                        aria-label="Payment State"
                        className="w-full rounded-lg border border-[#D4D4D4] bg-white px-2 py-4 text-sm outline-none"
                        value={paymentState}
                        onChange={(e: React.ChangeEvent<HTMLSelectElement>) =>
                          setPaymentState(
                            e.target.value as "settled" | "failed" | "inflight"
                          )
                        }
                      >
                        <option value="">Select State</option>
                        <option value="settled">Settled</option>
                        <option value="failed">Failed</option>
                        <option value="inflight">Inflight</option>
                      </select>
                    </div>
                  )}
                </div>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
